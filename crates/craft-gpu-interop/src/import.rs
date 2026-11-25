//! The compositor side: shared frames drawn into the caller's texture on its wgpu device.

use crate::fallback::Fallback;
use crate::ring::{Handoff, SharedFrame};

/// The pass that copies a texture into another of the same size with alpha set to 1 (`fs`), or
/// with its alpha kept (`fs_alpha`). The desktop's alpha channel is not reliable, so a captured
/// display is drawn opaque. A captured window keeps its alpha, as its copy through memory does;
/// the system delivers its pixels with the color multiplied by alpha, and `fs_alpha` divides it
/// out, since the compositor reads source textures with straight alpha.
const SHADER: &str = r"
@group(0) @binding(0) var src: texture_2d<f32>;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

@fragment
fn fs(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let c = textureLoad(src, vec2<i32>(pos.xy), 0);
    return vec4<f32>(c.rgb, 1.0);
}

@fragment
fn fs_alpha(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let c = textureLoad(src, vec2<i32>(pos.xy), 0);
    if c.a <= 0.0 {
        return vec4<f32>(0.0);
    }
    return vec4<f32>(min(c.rgb / c.a, vec3<f32>(1.0)), c.a);
}
";

/// An opaque copy between two textures of the same size: one full-screen triangle that reads
/// each source texel and writes it with alpha 1, or with its own alpha
/// ([`Opaque::draw_keeping_alpha`]). The pipeline for a target format is built on first use and
/// kept.
pub struct Opaque {
    layout: wgpu::BindGroupLayout,
    module: wgpu::ShaderModule,
    pipeline_layout: wgpu::PipelineLayout,
    /// The target format, whether alpha is kept, and the pipeline.
    pipelines: Vec<(wgpu::TextureFormat, bool, wgpu::RenderPipeline)>,
}

impl Opaque {
    pub fn new(device: &wgpu::Device) -> Opaque {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("opaque copy"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let module =
            device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("opaque copy"), source: wgpu::ShaderSource::Wgsl(SHADER.into()) });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("opaque copy"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        Opaque { layout, module, pipeline_layout, pipelines: Vec::new() }
    }

    fn pipeline(&mut self, device: &wgpu::Device, format: wgpu::TextureFormat, alpha: bool) -> &wgpu::RenderPipeline {
        let at = match self.pipelines.iter().position(|(f, a, _)| *f == format && *a == alpha) {
            Some(at) => at,
            None => {
                let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("opaque copy"),
                    layout: Some(&self.pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &self.module,
                        entry_point: Some("vs"),
                        compilation_options: Default::default(),
                        buffers: &[],
                    },
                    primitive: wgpu::PrimitiveState::default(),
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    fragment: Some(wgpu::FragmentState {
                        module: &self.module,
                        entry_point: Some(if alpha { "fs_alpha" } else { "fs" }),
                        compilation_options: Default::default(),
                        targets: &[Some(wgpu::ColorTargetState { format, blend: None, write_mask: wgpu::ColorWrites::ALL })],
                    }),
                    multiview_mask: None,
                    cache: None,
                });
                self.pipelines.push((format, alpha, pipeline));
                self.pipelines.len() - 1
            }
        };
        &self.pipelines[at].2
    }

    /// A bind group that reads `source`.
    pub fn bind(&self, device: &wgpu::Device, source: &wgpu::TextureView) -> wgpu::BindGroup {
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("opaque copy"),
            layout: &self.layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(source) }],
        })
    }

    /// Record the copy of the texture `source` reads into `target`, a render attachment of the
    /// same size in `format`, with alpha set to 1.
    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::BindGroup,
        target: &wgpu::TextureView,
        format: wgpu::TextureFormat,
    ) {
        self.record(device, encoder, source, target, format, false);
    }

    /// Record the copy as [`Opaque::draw`] does, with each texel's alpha kept and its color, which
    /// `source` holds multiplied by alpha, divided by alpha. A texel with alpha 0 is written as 0.
    pub fn draw_keeping_alpha(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::BindGroup,
        target: &wgpu::TextureView,
        format: wgpu::TextureFormat,
    ) {
        self.record(device, encoder, source, target, format, true);
    }

    fn record(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        source: &wgpu::BindGroup,
        target: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        alpha: bool,
    ) {
        let pipeline = self.pipeline(device, format, alpha);
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("opaque copy"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::BLACK), store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, source, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// What one call to [`Importer::draw`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drawn {
    /// No frame is waiting.
    Idle,
    /// The waiting frame's copy on the capture side has not finished; it is tried again on the
    /// next call.
    Waiting,
    /// The frame was drawn into the target, and its texture handed back to the capture side.
    Frame { width: u32, height: u32 },
}

/// A ring opened on the compositor's device. Windows opens a ring of shared textures, and Linux a
/// ring of DMA-BUFs, whose buffers are imported as they first arrive.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
struct Opened {
    ring: std::sync::Arc<crate::ring::SharedRing>,
    binds: Vec<Option<wgpu::BindGroup>>,
    #[cfg(windows)]
    raw: Raw,
    #[cfg(target_os = "linux")]
    raw: crate::dmabuf_ffi::Imported,
}

/// A ring's textures and fences on a Direct3D 12 or a Vulkan device.
#[cfg(windows)]
enum Raw {
    Dx12(crate::d3d12_ffi::Imported),
    /// Boxed: it holds the device's function table.
    Vulkan(Box<crate::vulkan_ffi::Imported>),
}

/// Shared frames of one source, drawn into the caller's texture. It opens a ring on first use
/// and keeps it open while the frames come from the same ring.
#[derive(Default)]
pub struct Importer {
    handoff: Handoff,
    opened: Option<Opened>,
    opaque: Option<Opaque>,
}

impl Importer {
    pub fn new() -> Importer {
        Importer::default()
    }

    /// Keep `frame` for the next [`Importer::draw`], replacing a frame not drawn yet.
    pub fn offer(&mut self, frame: SharedFrame) {
        self.handoff.offer(frame);
    }

    /// The frame waiting to be drawn.
    pub fn pending(&self) -> Option<&SharedFrame> {
        self.handoff.pending()
    }

    /// Forget the waiting frame, for a source that now shows something else.
    pub fn clear(&mut self) {
        self.handoff.clear();
    }

    /// Hand the textures whose draws have finished back to the capture side, without waiting.
    /// A compositor calls it on each frame on which it does not call [`Importer::draw`]; only a
    /// Vulkan import has anything to do. When the textures cannot be handed back,
    /// [`Fallback::ImportFailed`] is recorded on the ring, and the capture side goes back to a CPU
    /// copy instead of running out of textures.
    pub fn poll(&self) {
        if let Some(opened) = &self.opened
            && !poll(opened)
        {
            opened.ring.refuse(Fallback::ImportFailed);
        }
    }

    /// Draw the waiting frame and submit it on `queue`, without waiting for the GPU. `target` is
    /// called only once the frame is ready, with its width and height, and returns the view to
    /// draw into: a render attachment of that size in `format`. `None` skips the frame. A frame
    /// whose capture-side copy has not finished stays waiting. When the frame cannot be drawn on
    /// this device, the reason is recorded on its ring, the frame is dropped, and the reason is
    /// returned.
    pub fn draw(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        target: impl FnOnce(u32, u32) -> Option<wgpu::TextureView>,
    ) -> Result<Drawn, Fallback> {
        let Some(frame) = self.handoff.pending() else {
            self.poll();
            return Ok(Drawn::Idle);
        };
        let ring = std::sync::Arc::clone(frame.ring());
        if let Some(reason) = ring.fallback() {
            self.handoff.clear();
            return Err(reason);
        }
        if self.opened.as_ref().is_none_or(|o| o.ring.id() != ring.id()) {
            self.opened = None;
            match open(device, queue, &ring, self.opaque.get_or_insert_with(|| Opaque::new(device))) {
                Ok(o) => self.opened = Some(o),
                Err(reason) => {
                    ring.refuse(reason);
                    self.handoff.clear();
                    return Err(reason);
                }
            }
        }
        let Some(opened) = &mut self.opened else { return Ok(Drawn::Idle) };
        // A removed device's fence value counts no frame as done and records why on the ring;
        // textures that cannot be handed back record why too.
        if !poll(opened) {
            ring.refuse(Fallback::ImportFailed);
        }
        let done = ring.completed(self.handoff.pending().map_or(0, |f| completed(opened, f)));
        if let Some(reason) = ring.fallback() {
            self.handoff.clear();
            self.opened = None;
            return Err(reason);
        }
        // A frame whose texture the capture side took back is dropped, and the next one is tried.
        let frame = loop {
            match self.handoff.take_ready(done) {
                Some(frame) if frame.start_reading() => break frame,
                Some(_) => {}
                None if self.handoff.pending().is_some() => return Ok(Drawn::Waiting),
                None => return Ok(Drawn::Idle),
            }
        };
        let Some(opaque) = &mut self.opaque else { return Ok(Drawn::Idle) };
        if let Err(reason) = prepare(opened, device, &ring, opaque, frame.slot()) {
            ring.refuse(reason);
            self.opened = None;
            return Err(reason);
        }
        let Some(Some(bind)) = opened.binds.get(frame.slot()) else {
            ring.refuse(Fallback::ImportFailed);
            return Err(Fallback::ImportFailed);
        };
        let Some(target) = target(frame.width(), frame.height()) else {
            // The texture goes back to the capture side unread.
            release(opened, frame.ready())?;
            // Its DMA-BUF, and those of the frames before it, go back once the draws already
            // submitted have finished.
            #[cfg(target_os = "linux")]
            release_dmabuf_after_submitted(queue, &frame);
            return Ok(Drawn::Idle);
        };
        // wgpu does not let one encoder take both its own commands and raw ones, so the barriers
        // that pass the texture between the two sides are command buffers of their own.
        let before = acquire(opened, device, frame.slot());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("shared capture") });
        if ring.keeps_alpha() {
            opaque.draw_keeping_alpha(device, &mut encoder, bind, &target, format);
        } else {
            opaque.draw(device, &mut encoder, bind, &target, format);
        }
        let after = hand_back(opened, device, frame.slot());
        queue.submit(before.into_iter().chain([encoder.finish()]).chain(after));
        // A DMA-BUF goes back to its producer once the GPU has finished the draw that reads it.
        #[cfg(target_os = "linux")]
        release_dmabuf_after_submitted(queue, &frame);
        if let Err(reason) = release(opened, frame.ready()) {
            ring.refuse(reason);
            self.opened = None;
            return Err(reason);
        }
        Ok(Drawn::Frame { width: frame.width(), height: frame.height() })
    }
}

/// Open `ring` on `device`: on Direct3D 12, else on Vulkan.
#[cfg(windows)]
fn open(device: &wgpu::Device, queue: &wgpu::Queue, ring: &std::sync::Arc<crate::ring::SharedRing>, opaque: &Opaque) -> Result<Opened, Fallback> {
    let raw = match crate::d3d12_ffi::import(device, ring) {
        Err(Fallback::UnsupportedBackend) => Raw::Vulkan(Box::new(crate::vulkan_ffi::import(device, queue, ring)?)),
        imported => Raw::Dx12(imported?),
    };
    let textures = match &raw {
        Raw::Dx12(r) => &r.textures,
        Raw::Vulkan(r) => &r.textures,
    };
    let binds = textures.iter().map(|t| Some(opaque.bind(device, &t.create_view(&wgpu::TextureViewDescriptor::default())))).collect();
    Ok(Opened { ring: std::sync::Arc::clone(ring), binds, raw })
}

/// Open a ring of DMA-BUFs on `device`, which must run on Vulkan. Its buffers are imported by
/// [`prepare`] as their first frames are drawn. A ring of another kind cannot be opened here.
#[cfg(target_os = "linux")]
fn open(device: &wgpu::Device, _queue: &wgpu::Queue, ring: &std::sync::Arc<crate::ring::SharedRing>, _opaque: &Opaque) -> Result<Opened, Fallback> {
    if ring.dmabuf_layout().is_none() {
        return Err(Fallback::Unsupported);
    }
    let raw = crate::dmabuf_ffi::open(device, ring)?;
    Ok(Opened { ring: std::sync::Arc::clone(ring), binds: vec![None; ring.slots()], raw })
}

#[cfg(not(any(windows, target_os = "linux")))]
fn open(_device: &wgpu::Device, _queue: &wgpu::Queue, _ring: &std::sync::Arc<crate::ring::SharedRing>, _opaque: &Opaque) -> Result<Opened, Fallback> {
    Err(Fallback::Unsupported)
}

/// Record `frame` of a DMA-BUF ring as read once all the work submitted to `queue` so far has
/// finished on the GPU. The queue finishes its work in order, so every earlier frame has been read
/// by then too. The frame is marked as drawn first, so its buffer waits for that point.
#[cfg(target_os = "linux")]
pub(crate) fn release_dmabuf_after_submitted(queue: &wgpu::Queue, frame: &crate::ring::SharedFrame) {
    if frame.ring().dmabuf_layout().is_some() {
        frame.mark_drawn();
        let (ring, ready) = (std::sync::Arc::clone(frame.ring()), frame.ready());
        queue.on_submitted_work_done(move || ring.release_dmabuf(ready));
    }
}

/// Make texture `slot` of `ring` ready to draw: on Linux, import its buffer the first time.
#[cfg(target_os = "linux")]
fn prepare(opened: &mut Opened, device: &wgpu::Device, ring: &crate::ring::SharedRing, opaque: &Opaque, slot: usize) -> Result<(), Fallback> {
    if opened.binds.get(slot).is_some_and(Option::is_some) {
        return Ok(());
    }
    let texture = opened.raw.import(device, ring, slot)?;
    let bind = opaque.bind(device, &texture.create_view(&wgpu::TextureViewDescriptor::default()));
    let entry = opened.binds.get_mut(slot).ok_or(Fallback::ImportFailed)?;
    *entry = Some(bind);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn prepare(_opened: &mut Opened, _device: &wgpu::Device, _ring: &crate::ring::SharedRing, _opaque: &Opaque, _slot: usize) -> Result<(), Fallback> {
    Ok(())
}

/// The commands, submitted before the draw that reads texture `slot`, that pass the texture to the
/// compositor's queue. Only a Vulkan device has any.
#[cfg(windows)]
fn acquire(opened: &Opened, device: &wgpu::Device, slot: usize) -> Option<wgpu::CommandBuffer> {
    match &opened.raw {
        Raw::Vulkan(r) => r.acquire(device, slot),
        Raw::Dx12(_) => None,
    }
}

#[cfg(target_os = "linux")]
fn acquire(opened: &Opened, device: &wgpu::Device, slot: usize) -> Option<wgpu::CommandBuffer> {
    opened.raw.acquire(device, slot)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn acquire(_opened: &Opened, _device: &wgpu::Device, _slot: usize) -> Option<wgpu::CommandBuffer> {
    None
}

/// The commands, submitted after the draw that read texture `slot`, that pass the texture back to
/// the capture side. Only a Vulkan device has any.
#[cfg(windows)]
fn hand_back(opened: &Opened, device: &wgpu::Device, slot: usize) -> Option<wgpu::CommandBuffer> {
    match &opened.raw {
        Raw::Vulkan(r) => r.hand_back(device, slot),
        Raw::Dx12(_) => None,
    }
}

#[cfg(target_os = "linux")]
fn hand_back(opened: &Opened, device: &wgpu::Device, slot: usize) -> Option<wgpu::CommandBuffer> {
    opened.raw.hand_back(device, slot)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn hand_back(_opened: &Opened, _device: &wgpu::Device, _slot: usize) -> Option<wgpu::CommandBuffer> {
    None
}

/// The ready value up to which `frame`'s ring has finished writing, without waiting.
#[cfg(windows)]
fn completed(opened: &Opened, _frame: &SharedFrame) -> u64 {
    match &opened.raw {
        Raw::Dx12(r) => r.completed(),
        Raw::Vulkan(r) => r.completed(),
    }
}

/// On Linux: `frame`'s own value once its buffer has no pending write, else 0.
#[cfg(target_os = "linux")]
fn completed(_opened: &Opened, frame: &SharedFrame) -> u64 {
    crate::dmabuf::completed(frame)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn completed(_opened: &Opened, _frame: &SharedFrame) -> u64 {
    0
}

/// Copy the values the compositor's queue has reached to the released fence, without waiting;
/// false when that failed. Only a Vulkan import has anything to do.
#[cfg(windows)]
fn poll(opened: &Opened) -> bool {
    match &opened.raw {
        Raw::Vulkan(r) => r.poll(),
        Raw::Dx12(_) => true,
    }
}

#[cfg(not(windows))]
fn poll(_opened: &Opened) -> bool {
    true
}

/// Hand the texture of the frame with ready value `value`, drawn or skipped, back to the capture
/// side after the work already submitted to the compositor's queue.
#[cfg(windows)]
fn release(opened: &Opened, value: u64) -> Result<(), Fallback> {
    match &opened.raw {
        Raw::Dx12(r) => r.release(value),
        Raw::Vulkan(r) => {
            r.release(value);
            Ok(())
        }
    }
}

#[cfg(not(windows))]
fn release(_opened: &Opened, _value: u64) -> Result<(), Fallback> {
    Ok(())
}
