//! The compositor: GPU state and the per-frame passes.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::SyncSender;

use craft_gpu_yuv::{Conversion, Converter, Format, Matrix, Range};
use crossbeam_queue::ArrayQueue;
use studiocraft_media::Tick;
use studiocraft_media::clock::duration_of;
use studiocraft_media::queue::{Fifo, Latest};
use studiocraft_media::video::PixelFormat;
use studiocraft_model::{Crop, SourceId, Vec2};
use studiocraft_plugin::{EffectImage, VIDEO_EFFECT_PARAMS, VideoContent, VideoEffect, VideoOut};

use crate::nv12::{EncodeTarget, Nv12};
use crate::plan::{ItemDesc, MAX_THUMBNAILS, Plan, SceneDesc, ThumbnailRequest};
use crate::{CANVAS_FORMAT, CANVAS_VIEW_FORMAT, Gpu, RenderError, shaders};

/// Values in one uniform slot.
const SLOT_FLOATS: usize = 36;
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
    /// Start, change or stop drawing small pictures of scenes.
    pub thumbnails: Latest<Option<Arc<ThumbnailRequest>>>,
    /// New thumbnail textures for the interface, published when they are recreated.
    pub thumbnail_views: Latest<Arc<ThumbnailViews>>,
    /// Scenes of the newest thumbnail request past [`crate::MAX_THUMBNAILS`], which are not drawn.
    pub thumbnails_skipped: AtomicUsize,
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
            thumbnails: Latest::new(),
            thumbnail_views: Latest::new(),
            thumbnails_skipped: AtomicUsize::new(0),
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

/// Convert straight-alpha RGBA to premultiplied alpha in place, rounding to the nearest value.
fn premultiply(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        for c in &mut px[..3] {
            *c = ((u32::from(*c) * a + 127) / 255) as u8;
        }
    }
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

/// The small pictures of a [`ThumbnailRequest`]'s scenes, sampled through sRGB views like
/// [`CanvasViews`]. The textures are kept while requests come in; a request with more scenes or
/// another picture size replaces them.
pub struct ThumbnailViews {
    pub width: u32,
    pub height: u32,
    /// Changes whenever the textures are recreated.
    pub generation: u64,
    /// One ring per slot: slot `i` shows scene `i` of the request. There can be more slots than
    /// the newest request has scenes; those keep their last picture.
    pub slots: Vec<Ring>,
}

/// A thumbnail texture and the view the compositor draws into.
struct Plain {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

/// The thumbnail textures: a ring of [`RING_LEN`] for each slot, all of one size.
struct ThumbPool {
    targets: Vec<Vec<Plain>>,
    views: Arc<ThumbnailViews>,
}

/// A canvas-size render target and the bind group that samples it.
struct Target {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    /// The texture bound for the NV12 conversion.
    yuv: craft_gpu_yuv::Source,
}

/// A source's texture on the GPU.
struct Tex {
    texture: wgpu::Texture,
    /// The texture's whole view, which a shared capture is drawn into.
    view: wgpu::TextureView,
    bind: wgpu::BindGroup,
    width: u32,
    height: u32,
    format: wgpu::TextureFormat,
}

/// Two textures the filter passes of one source alternate between. Each pass draws into the
/// top-left corner; a texture is as large as the largest pass output.
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
    /// The frame the filter passes last ran, which texture of `fx` holds the result, and the
    /// result's size in pixels (0 when the filters left nothing to draw).
    fx_frame: u64,
    fx_result: usize,
    fx_size: (u32, u32),
    /// The capture textures shared with this device, for a source whose pictures stay on the GPU.
    shared: Option<craft_gpu_interop::Importer>,
}

/// A filter's picture on the GPU, uploaded once.
struct ImageTex {
    /// Held so the picture's id stays unique while the entry exists.
    _image: EffectImage,
    /// The texture and the bind group that samples it; `None` for a picture too large for the
    /// device, which was reported once.
    gpu: Option<(wgpu::Texture, wgpu::BindGroup)>,
}

impl ImageTex {
    fn bind(&self) -> Option<&wgpu::BindGroup> {
        self.gpu.as_ref().map(|(_, b)| b)
    }
}

/// The bind groups a pipeline reads after the uniforms.
#[derive(Clone, Copy)]
enum Layout {
    /// One texture.
    One,
    /// Two textures (transitions).
    Two,
    /// The input texture, and a filter's picture in group 2.
    Image,
}

struct Pipes {
    uniform_layout: wgpu::BindGroupLayout,
    tex_layout: wgpu::BindGroupLayout,
    tex2_layout: wgpu::BindGroupLayout,
    layout: wgpu::PipelineLayout,
    layout2: wgpu::PipelineLayout,
    layout_image: wgpu::PipelineLayout,
    item: wgpu::RenderPipeline,
    /// The program to NV12 for the encoders and the scopes.
    yuv: Converter,
    effects: HashMap<&'static str, Option<wgpu::RenderPipeline>>,
    /// The effect keys compiled to read a picture.
    image_effects: HashSet<&'static str>,
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
    /// The pictures the plan's filters read, by [`EffectImage::id`].
    images: HashMap<usize, ImageTex>,
    /// When the plan's animations started, by [`studiocraft_plugin::Animation::id`]: `None` until
    /// a frame draws them. `set_plan` makes the entries, so frames do not allocate.
    started: HashMap<u64, Option<Tick>>,
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
    /// The scenes drawn as small pictures, if any.
    thumbs: Option<Arc<ThumbnailRequest>>,
    thumb_pool: Option<ThumbPool>,
    thumb_generation: u64,
    /// When each thumbnail slot was last drawn into its current textures.
    thumb_last: [Option<Tick>; MAX_THUMBNAILS],
    /// The slots the newest request has not drawn yet.
    thumb_fresh: [bool; MAX_THUMBNAILS],
}

/// Most thumbnails one frame draws. Due thumbnails wait for later frames, oldest first, so their
/// cost is spread over frames instead of landing on one.
const THUMBNAILS_PER_FRAME: usize = 2;

/// The thumbnail slots a frame draws: the first `.1` entries of `.0`.
type Picks = ([usize; THUMBNAILS_PER_FRAME], usize);

/// Whether two scenes draw the same sources in the same order; their transforms and filters may
/// differ.
fn same_sources(a: &SceneDesc, b: &SceneDesc) -> bool {
    a.items.len() == b.items.len() && a.items.iter().zip(&b.items).all(|(x, y)| x.source == y.source)
}

/// The items of the slots in `picks`.
fn picked_items<'a>(req: &'a ThumbnailRequest, picks: &'a [usize]) -> impl Iterator<Item = &'a ItemDesc> {
    picks.iter().filter_map(|&s| req.drawn().get(s)).flat_map(|scene| scene.items.iter())
}

/// Largest canvas side.
pub const MAX_CANVAS: u32 = 8192;

/// The distance between uniform slots: the slot's size rounded up to the device's offset
/// alignment, so every dynamic offset is a multiple of it.
fn slot_stride(alignment: u32) -> u64 {
    let align = u64::from(alignment.max(1));
    SLOT_BYTES.div_ceil(align) * align
}

fn clamp_canvas(width: u32, height: u32, limit: u32) -> (u32, u32) {
    let max = limit.min(MAX_CANVAS);
    (width.clamp(16, max), height.clamp(16, max))
}

impl Compositor {
    /// A compositor drawing a `width` x `height` canvas.
    pub fn new(gpu: Gpu, width: u32, height: u32) -> Result<Compositor, RenderError> {
        let pipes = gpu.scoped("compositor", || make_pipes(&gpu))?;
        let stride = slot_stride(gpu.device.limits().min_uniform_buffer_offset_alignment);
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
            images: HashMap::new(),
            started: HashMap::new(),
            plan: Arc::new(Plan::default()),
            nv12: None,
            scope_nv12: None,
            scope_skipped: AtomicU64::new(0),
            frame_no: 0,
            errors: Vec::new(),
            waiting: Vec::with_capacity(CAPTURE_SLOTS),
            thumbs: None,
            thumb_pool: None,
            thumb_generation: 0,
            thumb_last: [None; MAX_THUMBNAILS],
            thumb_fresh: [false; MAX_THUMBNAILS],
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
        self.ensure_thumb_pool();
    }

    /// Draw `plan` from the next frame on, compiling the shaders and uploading the filter
    /// pictures it needs first. Pictures that neither the plan nor the thumbnails use are released.
    pub fn set_plan(&mut self, plan: Arc<Plan>) {
        if let Some(t) = &plan.transition
            && !self.pipes.transitions.contains_key(&t.key)
        {
            let p = self.compile(&t.key, &shaders::transition(t.wgsl), Layout::Two, "fs_transition");
            self.pipes.transitions.insert(t.key.clone(), p);
        }
        self.plan = plan;
        self.prepare();
    }

    /// Start, change or stop drawing small pictures of scenes, compiling the shaders and uploading
    /// the filter pictures they need first. Each frame draws at most two pictures, oldest first.
    /// A picture is redrawn at most at the request's rate, counted from the last time its slot was
    /// drawn, so a new request does not draw sooner, unless the slot now shows other sources or
    /// another key (a reordered or replaced scene). A request that draws once draws each slot
    /// once, over the next frames.
    pub fn set_thumbnails(&mut self, request: Option<Arc<ThumbnailRequest>>) {
        // A slot that now shows other sources or another key is drawn again soon, not after the
        // rate's interval.
        if let Some(new) = &request {
            for (s, scene) in new.drawn().iter().enumerate() {
                let old_req = self.thumbs.as_ref();
                let old = old_req.and_then(|t| t.drawn().get(s));
                let same_key = old_req.is_some_and(|t| t.keys.get(s) == new.keys.get(s));
                if !(same_key && old.is_some_and(|old| same_sources(old, scene))) {
                    self.thumb_last[s] = None;
                }
            }
        }
        self.thumbs = request;
        self.thumb_fresh = [true; MAX_THUMBNAILS];
        self.prepare();
        self.ensure_thumb_pool();
    }

    /// The thumbnail textures, once a request with scenes has been made.
    pub fn thumbnail_views(&self) -> Option<Arc<ThumbnailViews>> {
        self.thumb_pool.as_ref().map(|p| Arc::clone(&p.views))
    }

    /// Scenes of the current thumbnail request past [`crate::MAX_THUMBNAILS`], which are not drawn.
    pub fn thumbnails_skipped(&self) -> usize {
        self.thumbs.as_ref().map_or(0, |t| t.skipped())
    }

    /// Compile the shaders and upload the filter pictures that the plan and the thumbnail request
    /// need, and release the pictures and animation clocks neither uses.
    fn prepare(&mut self) {
        let plan = Arc::clone(&self.plan);
        let thumbs = self.thumbs.clone();
        let mut used = HashSet::new();
        let mut animations = HashSet::new();
        for item in plan.items().chain(thumbs.iter().flat_map(|t| t.items())) {
            for fx in &item.effects {
                if let Some(a) = &fx.animation {
                    animations.insert(a.id());
                }
                if !self.pipes.effects.contains_key(fx.key) {
                    let p = match fx.image {
                        Some(_) => self.compile(fx.key, &shaders::effect_with_image(fx.wgsl), Layout::Image, "fs_effect"),
                        None => self.compile(fx.key, &shaders::effect(fx.wgsl), Layout::One, "fs_effect"),
                    };
                    if fx.image.is_some() {
                        self.pipes.image_effects.insert(fx.key);
                    }
                    self.pipes.effects.insert(fx.key, p);
                }
                if let Some(image) = &fx.image {
                    used.insert(image.id());
                    self.upload_image(image);
                }
            }
        }
        self.images.retain(|id, _| used.contains(id));
        self.started.retain(|id, _| animations.contains(id));
        for id in animations {
            self.started.entry(id).or_insert(None);
        }
    }

    /// Put `image` on the GPU unless it is there already. A picture larger than the device's
    /// textures is reported and left out, and the passes that read it are skipped.
    fn upload_image(&mut self, image: &EffectImage) {
        if self.images.contains_key(&image.id()) {
            return;
        }
        let device = &self.gpu.device;
        let limit = device.limits().max_texture_dimension_2d;
        if image.width() > limit || image.height() > limit {
            self.errors.push(RenderError::Device(format!(
                "a filter's {} x {} picture is larger than the graphics device's {limit} pixels",
                image.width(),
                image.height()
            )));
            self.images.insert(image.id(), ImageTex { _image: image.clone(), gpu: None });
            return;
        }
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
        let texture = texture(device, "filter picture", image.width(), image.height(), format, usage, &[]);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = tex_bind(device, &self.pipes, &view);
        let tex = Tex { texture, view, bind, width: image.width(), height: image.height(), format };
        // Premultiplied, so the linear filter does not mix the color of transparent pixels in.
        let mut rgba = image.rgba().to_vec();
        premultiply(&mut rgba);
        write(&self.gpu.queue, &tex, &rgba);
        self.images.insert(image.id(), ImageTex { _image: image.clone(), gpu: Some((tex.texture, tex.bind)) });
    }

    fn compile(&mut self, key: &str, source: &str, layout: Layout, fs: &str) -> Option<wgpu::RenderPipeline> {
        let layout = match layout {
            Layout::One => &self.pipes.layout,
            Layout::Two => &self.pipes.layout2,
            Layout::Image => &self.pipes.layout_image,
        };
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
        self.nv12 = retarget(&self.gpu.device, &self.pipes.yuv, self.nv12.take(), target, &mut self.errors);
    }

    /// Start, change or stop the scopes' NV12 copy of the program.
    pub fn set_scope(&mut self, target: Option<EncodeTarget>) {
        self.scope_nv12 = retarget(&self.gpu.device, &self.pipes.yuv, self.scope_nv12.take(), target, &mut self.errors);
    }

    /// Frames the scopes' copy skipped because all its read-back buffers were in flight.
    pub fn scope_skipped(&self) -> u64 {
        self.scope_skipped.load(Ordering::Relaxed)
    }

    /// Apply what the app published, then draw a frame. On a lost device nothing is applied or
    /// drawn, and every capture is answered with the loss.
    pub fn run(&mut self, shared: &RenderShared, now: Tick) {
        if let Some(why) = self.gpu.lost() {
            while let Some(req) = shared.captures.pop() {
                self.waiting.push(req);
            }
            for req in self.waiting.drain(..) {
                let _ = req.reply.try_send(Err(RenderError::Device(format!("the graphics device was lost: {why}"))));
            }
            return;
        }
        let thumb_generation = self.thumb_generation;
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
        if let Some(t) = shared.thumbnails.take() {
            self.set_thumbnails(t);
            shared.thumbnails_skipped.store(self.thumbnails_skipped(), Ordering::Relaxed);
        }
        for e in self.errors.drain(..) {
            shared.errors.push(e);
        }
        self.frame(now, &shared.readback_lost);
        shared.scope_skipped.store(self.scope_skipped(), Ordering::Relaxed);
        if self.thumb_generation != thumb_generation
            && let Some(views) = self.thumbnail_views()
        {
            shared.thumbnail_views.publish(views);
        }
        let mut waiting = std::mem::take(&mut self.waiting);
        for req in waiting.drain(..) {
            // A caller that gave up has dropped its receiver; nothing waits for the answer.
            let _ = req.reply.try_send(self.capture(req.view));
        }
        self.waiting = waiting;
    }

    /// The newest picture of `view` with straight alpha. Waits for the GPU.
    pub fn capture(&self, view: View) -> Result<Capture, RenderError> {
        if let Some(why) = self.gpu.lost() {
            return Err(RenderError::Device(format!("the graphics device was lost: {why}")));
        }
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
        // A uniform slot for each item and effect, and one for the transition. The NV12
        // conversions keep their own uniforms.
        let mut slots = u64::from(plan.transition.is_some());
        for item in plan.items() {
            slots += 1 + item.effects.len() as u64;
            self.sync_source(item);
        }
        // Sources seen only in thumbnails are uploaded only on the frames that draw them.
        let picks = self.thumbnails_due(now);
        let picks = &picks.0[..picks.1];
        let thumbs = if picks.is_empty() { None } else { self.thumbs.clone() };
        if let Some(req) = &thumbs {
            for item in picked_items(req, picks) {
                slots += 1 + item.effects.len() as u64;
                self.sync_source(item);
            }
        }
        if self.frame_no.is_multiple_of(SWEEP_FRAMES) {
            self.sources.retain(|_, s| Arc::strong_count(&s.video) > 1);
        }
        let device = self.gpu.device.clone();
        self.uniforms.begin(&device, &self.pipes.uniform_layout, slots);
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });

        for item in plan.items().chain(thumbs.iter().flat_map(|t| picked_items(t, picks))) {
            if !item.effects.is_empty()
                && let Some(st) = self.sources.get_mut(&item.source)
            {
                let clock = EffectClock { now, started: &mut self.started };
                run_effects(&device, &self.pipes, &self.images, clock, &mut self.uniforms, &mut enc, st, &item.effects, self.frame_no);
            }
        }

        let ctx = Ctx { pipes: &self.pipes, sources: &self.sources, frame_no: self.frame_no, canvas: (self.width as f32, self.height as f32) };
        let pi = self.views.program.latest().map_or(0, |i| (i + 1) % RING_LEN);
        let program = &self.program[pi];
        let transition = plan.transition.as_ref().and_then(|t| {
            let p = t.progress(now);
            let pipe = self.pipes.transitions.get(&t.key)?.as_ref()?;
            t.running(now).then_some((t, p, pipe))
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

        if let (Some(req), Some(pool)) = (&thumbs, &self.thumb_pool) {
            for &s in picks {
                let (Some(scene), Some(ring), Some(targets)) = (req.drawn().get(s), pool.views.slots.get(s), pool.targets.get(s)) else { continue };
                let i = ring.latest().map_or(0, |i| (i + 1) % RING_LEN);
                draw_scene(&ctx, &mut self.uniforms, &mut enc, &targets[i].view, scene);
            }
        }

        let copied = self.nv12.as_mut().is_some_and(|n| convert(n, &self.pipes.yuv, &mut enc, program, now, readback_lost));
        let scope_copied = self.scope_nv12.as_mut().is_some_and(|n| convert(n, &self.pipes.yuv, &mut enc, program, now, &self.scope_skipped));

        self.uniforms.flush(&self.gpu.queue);
        self.gpu.queue.submit([enc.finish()]);
        self.views.program.publish(pi);
        if let Some(i) = qi {
            self.views.preview.publish(i);
        }
        if thumbs.is_some() {
            self.thumbnails_drawn(picks, now);
        }
        for (n, copied) in [(&mut self.nv12, copied), (&mut self.scope_nv12, scope_copied)] {
            if copied && let Some(n) = n {
                n.submitted();
            }
        }
        if self.nv12.is_some() || self.scope_nv12.is_some() {
            self.gpu.poll();
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
        let fresh = |video: &Arc<VideoOut>| SourceTex {
            video: Arc::clone(video),
            tex: None,
            native: (0, 0),
            solid: None,
            fx: None,
            fx_frame: 0,
            fx_result: 0,
            fx_size: (0, 0),
            shared: None,
        };
        let st = self.sources.entry(item.source).or_insert_with(|| fresh(&item.video));
        if !Arc::ptr_eq(&st.video, &item.video) {
            *st = fresh(&item.video);
        }
        let (device, queue) = (&self.gpu.device, &self.gpu.queue);
        let content = st.video.take();
        if content.is_some()
            && !matches!(content, Some(VideoContent::Shared(_)))
            && let Some(importer) = &mut st.shared
        {
            // A picture from memory, a color or nothing replaces a shared picture still waiting.
            importer.clear();
        }
        if let Some(VideoContent::Shared(f)) = content {
            st.shared.get_or_insert_with(Default::default).offer(f);
        } else if let Some(content) = content {
            Self::sync_content(device, queue, &self.pipes, st, content);
        }
        if st.shared.as_ref().is_some_and(|i| i.pending().is_some()) {
            Self::draw_shared(device, queue, &self.pipes, st);
        } else if let Some(importer) = &st.shared {
            // Shared textures whose draws have finished go back to the capture.
            importer.poll();
        }
    }

    /// Draw a source's waiting shared picture into its texture, without waiting for the capture's
    /// copy. When the device cannot take it, the reason is on the picture's ring and the source
    /// sends its next pictures from memory; the texture keeps the last picture until then.
    fn draw_shared(device: &wgpu::Device, queue: &wgpu::Queue, pipes: &Pipes, st: &mut SourceTex) {
        let Some(importer) = &mut st.shared else { return };
        let Some(size) = importer.pending().and_then(|f| shared_size(device, f)) else {
            if let Some(f) = importer.pending() {
                f.ring().refuse(craft_gpu_interop::Fallback::Format);
            }
            importer.clear();
            return;
        };
        let format = wgpu::TextureFormat::Bgra8Unorm;
        // The texture is made or replaced only for a picture that is ready, so the last picture
        // stays while the capture's copy runs or when the device cannot take it.
        let slot = &mut st.tex;
        let target = |_w, _h| Some(shared_target(device, pipes, slot, size, format));
        match importer.draw(device, queue, format, target) {
            Ok(craft_gpu_interop::Drawn::Frame { width, height }) => {
                st.native = (width, height);
                st.solid = None;
            }
            Ok(craft_gpu_interop::Drawn::Waiting | craft_gpu_interop::Drawn::Idle) => {}
            Err(reason) => log::debug!("a shared capture is copied through memory: {reason}"),
        }
    }

    /// Bring a source's texture up to date with content from memory, a color or nothing.
    fn sync_content(device: &wgpu::Device, queue: &wgpu::Queue, pipes: &Pipes, st: &mut SourceTex, content: VideoContent) {
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
                let tex = ensure_tex(device, pipes, &mut st.tex, f.width, f.height, format);
                write(queue, tex, &f.data);
                st.native = (f.width, f.height);
                st.solid = None;
            }
            VideoContent::Solid { rgba, width, height } => {
                let fresh = st.tex.as_ref().is_none_or(|t| (t.width, t.height, t.format) != (1, 1, wgpu::TextureFormat::Rgba8Unorm));
                let tex = ensure_tex(device, pipes, &mut st.tex, 1, 1, wgpu::TextureFormat::Rgba8Unorm);
                if fresh || st.solid != Some(rgba) {
                    write(queue, tex, &rgba);
                }
                st.native = (width.max(1), height.max(1));
                st.solid = Some(rgba);
            }
            VideoContent::Empty => st.native = (0, 0),
            // Taken by `sync_source` before this is called.
            VideoContent::Shared(_) => {}
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

    /// Read the newest picture of thumbnail slot `slot` back (premultiplied alpha), if one was
    /// drawn. Waits for the GPU; meant for tests and benchmarks.
    pub fn read_thumbnail(&self, slot: usize) -> Result<Option<Vec<u8>>, RenderError> {
        let Some(pool) = &self.thumb_pool else { return Ok(None) };
        let Some(i) = pool.views.slots.get(slot).and_then(Ring::latest) else { return Ok(None) };
        let target = pool.targets.get(slot).and_then(|t| t.get(i)).ok_or_else(|| RenderError::Device("no thumbnail picture".into()))?;
        read_rgba(&self.gpu, &target.texture, pool.views.width, pool.views.height).map(Some)
    }

    /// Record that this frame drew the thumbnail slots `picks` at `now`. A slot whose scene has a
    /// shared picture still waiting for its copy stays due for a request that draws once, so the
    /// slot is drawn again when the picture is ready.
    fn thumbnails_drawn(&mut self, picks: &[usize], now: Tick) {
        let (Some(req), Some(pool)) = (&self.thumbs, &self.thumb_pool) else { return };
        for &s in picks {
            if let Some(ring) = pool.views.slots.get(s) {
                ring.publish(ring.latest().map_or(0, |i| (i + 1) % RING_LEN));
                self.thumb_last[s] = Some(now);
                self.thumb_fresh[s] = req.drawn().get(s).is_some_and(|scene| waits_for_shared(&self.sources, scene));
            }
        }
    }

    /// The thumbnail slots this frame draws, at most [`THUMBNAILS_PER_FRAME`], oldest first. A
    /// slot is due when it has not been drawn into its textures, when the request's interval has
    /// passed since its last drawing (or the clock went back), or, for a request that draws once,
    /// when the request has not drawn it.
    fn thumbnails_due(&self, now: Tick) -> Picks {
        let mut picks: Picks = ([0; THUMBNAILS_PER_FRAME], 0);
        let Some(req) = &self.thumbs else { return picks };
        let interval = req.interval();
        let due = |s: usize| match (interval, self.thumb_last[s]) {
            (_, None) => true,
            (None, Some(_)) => self.thumb_fresh[s],
            (Some(interval), Some(last)) => now.0 < last.0 || now.0.saturating_sub(last.0) >= interval.0,
        };
        while picks.1 < THUMBNAILS_PER_FRAME {
            let taken = &picks.0[..picks.1];
            let next =
                (0..req.drawn().len()).filter(|&s| due(s) && !taken.contains(&s)).min_by_key(|&s| self.thumb_last[s].map_or(i64::MIN, |t| t.0));
            let Some(s) = next else { break };
            picks.0[picks.1] = s;
            picks.1 += 1;
        }
        picks
    }

    /// Make the thumbnail textures fit the request: its picture size at the current canvas, and a
    /// slot for each scene it draws. Textures that already fit are kept, so frames never make
    /// them; textures made again hold nothing, and each slot is due at once.
    fn ensure_thumb_pool(&mut self) {
        let Some(req) = self.thumbs.clone().filter(|r| !r.drawn().is_empty()) else { return };
        let (w, h) = req.picture_size((self.width, self.height), self.gpu.device.limits().max_texture_dimension_2d);
        let count = req.drawn().len();
        let kept = self.thumb_pool.as_ref().filter(|p| (p.views.width, p.views.height) == (w, h));
        if kept.is_some_and(|p| p.targets.len() >= count) {
            return;
        }
        let count = count.max(kept.map_or(0, |p| p.targets.len()));
        self.thumb_generation += 1;
        self.thumb_pool = Some(make_thumb_pool(&self.gpu, w, h, count, self.thumb_generation));
        self.thumb_last = [None; MAX_THUMBNAILS];
    }

    /// Wait until the GPU has finished every frame submitted so far and every read-back's
    /// callback has run, and hand finished read-backs to the encoders. Another thread polling the
    /// same device (the window) may run a callback after the GPU wait returns; this waits for it
    /// for at most `MAP_WAIT`.
    pub fn finish(&mut self, readback_lost: &AtomicU64) {
        if self.gpu.wait().is_ok() {
            for n in [&self.nv12, &self.scope_nv12].into_iter().flatten() {
                n.wait_mapped(MAP_WAIT);
            }
        }
        if let Some(n) = &mut self.nv12 {
            n.collect(readback_lost);
        }
        if let Some(n) = &mut self.scope_nv12 {
            n.collect(&self.scope_skipped);
        }
    }
}

/// `current` sending to `target`'s sinks when it already has `target`'s size, else a new converter,
/// or none without a target. A size the device cannot hold is an error in `errors`, and no
/// converter.
fn retarget(
    device: &wgpu::Device,
    converter: &Converter,
    current: Option<Nv12>,
    target: Option<EncodeTarget>,
    errors: &mut Vec<RenderError>,
) -> Option<Nv12> {
    match (target, current) {
        (None, _) => None,
        (Some(t), Some(mut n)) if (n.width, n.height) == crate::nv12::even_size(&t) => {
            n.set_sinks(t.sinks);
            Some(n)
        }
        (Some(t), _) => match Nv12::new(device, converter, t) {
            Ok(n) => Some(n),
            Err(e) => {
                let e = RenderError::Device(format!("frames for the encoders cannot be converted: {e}"));
                log::warn!("{e}");
                errors.push(e);
                None
            }
        },
    }
}

/// The program to NV12 (BT.709, limited range) converter.
pub(crate) fn yuv_converter(device: &wgpu::Device) -> Converter {
    Converter::new(device, Conversion::new(Format::Nv12, Matrix::Bt709, Range::Limited))
}

/// Record `n`'s two conversion passes from `program` and its copy into a free read-back buffer;
/// whether it was recorded. With every buffer in flight the frame is skipped and counted in `lost`.
fn convert(n: &mut Nv12, converter: &Converter, enc: &mut wgpu::CommandEncoder, program: &Target, now: Tick, lost: &AtomicU64) -> bool {
    if !n.has_room() {
        lost.fetch_add(1, Ordering::Relaxed);
        return false;
    }
    if converter.draw(enc, &program.yuv, &n.planes).is_err() || !n.copy(enc, now) {
        lost.fetch_add(1, Ordering::Relaxed);
        return false;
    }
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
    gpu.wait()?;
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
        if filtered && (st.fx_size.0 == 0 || st.fx_size.1 == 0) {
            continue;
        }
        let (bind, straight, size, uv) = match (&st.fx, filtered) {
            (Some(fx), true) => {
                // The result fills the top-left `fx_size` of the filter texture.
                let size = st.fx_size;
                let [u0, v0, u1, v1] = crop_uv(item.transform.crop, size);
                let (su, sv) = (size.0 as f32 / fx.width.max(1) as f32, size.1 as f32 / fx.height.max(1) as f32);
                (&fx.binds[st.fx_result], 0.0, size, [u0 * su, v0 * sv, u1 * su, v1 * sv])
            }
            _ => {
                let uv = if st.solid.is_some() { [0.0, 0.0, 1.0, 1.0] } else { crop_uv(item.transform.crop, st.native) };
                (&tex.bind, 1.0, st.native, uv)
            }
        };
        if size.0 == 0 || size.1 == 0 {
            continue;
        }
        let t = item.transform.sanitized();
        let [c0, c1, c2, c3] = t.corners(size).map(clip);
        let slot = [c0[0], c0[1], c1[0], c1[1], c2[0], c2[1], c3[0], c3[1], uv[0], uv[1], uv[2], uv[3], straight, 1.0, 0.0, 0.0];
        let off = uniforms.push(&slot);
        pass.set_bind_group(0, &uniforms.bind, &[off]);
        pass.set_bind_group(1, bind, &[]);
        pass.draw(0..6, 0..1);
    }
}

/// Whether `e` can run: its pipeline compiled, it has a picture exactly when its key was
/// compiled with one, and its picture is on the GPU.
fn ready(pipes: &Pipes, images: &HashMap<usize, ImageTex>, e: &VideoEffect) -> bool {
    let compiled = matches!(pipes.effects.get(e.key), Some(Some(_)));
    let with_image = pipes.image_effects.contains(e.key);
    compiled
        && match &e.image {
            None => !with_image,
            Some(image) => with_image && images.get(&image.id()).and_then(ImageTex::bind).is_some(),
        }
}

/// The frame's time and when each animation started.
struct EffectClock<'a> {
    now: Tick,
    started: &'a mut HashMap<u64, Option<Tick>>,
}

impl EffectClock<'_> {
    /// `e`'s parameters this frame, for an input of `input` pixels.
    fn params(&mut self, e: &VideoEffect, input: (u32, u32)) -> [f32; VIDEO_EFFECT_PARAMS] {
        let mut params = e.params;
        if let Some(a) = &e.animation {
            // An animation the plan did not hold stays at its start.
            let start = self.started.get_mut(&a.id()).map_or(self.now, |s| *s.get_or_insert(self.now));
            let seconds = duration_of(Tick(self.now.0.saturating_sub(start.0))).as_secs_f64();
            a.step(&mut params, seconds, input);
        }
        params
    }
}

/// Run a source's filter passes once this frame.
#[allow(clippy::too_many_arguments)]
fn run_effects(
    device: &wgpu::Device,
    pipes: &Pipes,
    images: &HashMap<usize, ImageTex>,
    mut clock: EffectClock<'_>,
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
    let limit = device.limits().max_texture_dimension_2d;
    let side = |v: u32| v.min(limit);
    // A solid's 1 x 1 texture stands for its whole size, which can be larger than a texture;
    // only the passes' outputs are limited.
    let source = if st.solid.is_some() { st.native } else { (tex.width, tex.height) };
    let runnable = || effects.iter().filter(|e| ready(pipes, images, e));
    // The size each pass draws at, and the largest of them, which the filter textures need.
    let mut size = source;
    let mut largest = (1, 1);
    let mut any = false;
    for e in runnable() {
        if size.0 > 0 && size.1 > 0 {
            let (ow, oh) = e.reframe.size(size);
            size = (side(ow), side(oh));
            largest = (largest.0.max(size.0), largest.1.max(size.1));
        }
        any = true;
    }
    if !any {
        return;
    }
    st.fx_frame = frame_no;
    if size.0 == 0 || size.1 == 0 || source.0 == 0 || source.1 == 0 {
        // Nothing is left to draw.
        st.fx_size = (0, 0);
        return;
    }
    if st.fx.as_ref().is_none_or(|f| (f.width, f.height) != largest) {
        st.fx = Some(make_fx(device, pipes, largest.0, largest.1));
    }
    let Some(fx) = &st.fx else { return };
    let mut input = &tex.bind;
    // The input's size in pixels and its extent in the texture it is read from.
    let mut in_size = source;
    let mut extent = [1.0f32, 1.0];
    let mut straight = 1.0;
    let mut out = 0;
    for e in runnable() {
        let Some(Some(pipe)) = pipes.effects.get(e.key) else { continue };
        let (left, top, ow, oh) = e.reframe.apply(in_size);
        let (ow, oh) = (side(ow), side(oh));
        let (iw, ih) = (in_size.0 as f32, in_size.1 as f32);
        // The texture rectangle under the output: input pixel `left + ox` at output pixel `ox`.
        let (u0, v0) = (left as f32 / iw * extent[0], top as f32 / ih * extent[1]);
        let (u1, v1) = ((left as f32 + ow as f32) / iw * extent[0], (top as f32 + oh as f32) / ih * extent[1]);
        let mut slot = [0.0; SLOT_FLOATS];
        slot[..8].copy_from_slice(&[u0, v0, u1, v1, ow as f32, oh as f32, straight, 0.0]);
        slot[8..32].copy_from_slice(&clock.params(e, in_size));
        slot[32..].copy_from_slice(&[extent[0], extent[1], iw, ih]);
        let off = uniforms.push(&slot);
        let mut pass = begin(enc, &fx.views[out], Some(wgpu::Color::TRANSPARENT));
        pass.set_viewport(0.0, 0.0, ow as f32, oh as f32, 0.0, 1.0);
        pass.set_pipeline(pipe);
        pass.set_bind_group(0, &uniforms.bind, &[off]);
        pass.set_bind_group(1, input, &[]);
        if let Some(bind) = e.image.as_ref().and_then(|i| images.get(&i.id())).and_then(ImageTex::bind) {
            pass.set_bind_group(2, bind, &[]);
        }
        pass.draw(0..3, 0..1);
        drop(pass);
        input = &fx.binds[out];
        in_size = (ow, oh);
        extent = [ow as f32 / fx.width as f32, oh as f32 / fx.height as f32];
        straight = 0.0;
        st.fx_result = out;
        out = 1 - out;
    }
    st.fx_size = in_size;
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
        // Uploads copy into the texture; a shared capture is drawn into it.
        let usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::RENDER_ATTACHMENT;
        let texture = texture(device, "source", w, h, format, usage, &[]);
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind = tex_bind(device, pipes, &view);
        Tex { texture, view, bind, width: w, height: h, format }
    })
}

/// The view a shared picture of `size` is drawn into: the view of the source's texture, which
/// is made only when the texture is.
fn shared_target(device: &wgpu::Device, pipes: &Pipes, slot: &mut Option<Tex>, size: (u32, u32), format: wgpu::TextureFormat) -> wgpu::TextureView {
    ensure_tex(device, pipes, slot, size.0, size.1, format).view.clone()
}

/// Whether a source of `scene` has a shared picture still waiting for its copy.
fn waits_for_shared(sources: &HashMap<SourceId, SourceTex>, scene: &SceneDesc) -> bool {
    scene.items.iter().any(|i| sources.get(&i.source).and_then(|s| s.shared.as_ref()).is_some_and(|s| s.pending().is_some()))
}

/// A shared picture's size, when it is not empty and fits the device's textures.
fn shared_size(device: &wgpu::Device, f: &craft_gpu_interop::SharedFrame) -> Option<(u32, u32)> {
    let limit = device.limits().max_texture_dimension_2d;
    let (w, h) = (f.width(), f.height());
    (w > 0 && h > 0 && w <= limit && h <= limit).then_some((w, h))
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
    let yuv = pipes.yuv.bind(&gpu.device, &view);
    (Target { texture, view, yuv }, srgb)
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

fn make_thumb_pool(gpu: &Gpu, w: u32, h: u32, count: usize, generation: u64) -> ThumbPool {
    let usage = wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC;
    let (targets, slots) = (0..count)
        .map(|_| {
            let (targets, views): (Vec<_>, Vec<_>) = (0..RING_LEN)
                .map(|_| {
                    let texture = texture(&gpu.device, "scene thumbnail", w, h, CANVAS_FORMAT, usage, &[CANVAS_VIEW_FORMAT]);
                    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let srgb = texture.create_view(&wgpu::TextureViewDescriptor { format: Some(CANVAS_VIEW_FORMAT), ..Default::default() });
                    (Plain { texture, view }, srgb)
                })
                .unzip();
            (targets, Ring { views, latest: AtomicUsize::new(0), drawn: AtomicBool::new(false) })
        })
        .unzip();
    ThumbPool { targets, views: Arc::new(ThumbnailViews { width: w, height: h, generation, slots }) }
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
    let layout_image = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("texture and picture"),
        bind_group_layouts: &[Some(&uniform_layout), Some(&tex_layout), Some(&tex_layout)],
        immediate_size: 0,
    });
    let module = |label: &str, source: String| {
        device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some(label), source: wgpu::ShaderSource::Wgsl(source.into()) })
    };
    let item_module = module("items", shaders::item());
    // Premultiplied alpha over what is already drawn.
    let over = wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING;
    let item = pipeline(device, "items", &layout, &item_module, "vs_item", "fs_item", CANVAS_FORMAT, Some(over));
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
        layout_image,
        item,
        yuv: yuv_converter(device),
        effects: HashMap::new(),
        image_effects: HashSet::new(),
        transitions: HashMap::new(),
        sampler,
    }
}

#[cfg(test)]
mod tests;
