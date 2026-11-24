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

/// A ring opened on the compositor's device. Only Windows opens one.
#[cfg_attr(not(windows), allow(dead_code))]
struct Opened {
    ring: u64,
    binds: Vec<wgpu::BindGroup>,
    #[cfg(windows)]
    raw: Raw,
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
    /// Vulkan import has anything to do.
    pub fn poll(&self) {
        if let Some(opened) = &self.opened {
            poll(opened);
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
        if self.opened.as_ref().is_none_or(|o| o.ring != ring.id()) {
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
        let Some(opened) = &self.opened else { return Ok(Drawn::Idle) };
        let Some(frame) = self.handoff.take_ready(completed(opened)) else { return Ok(Drawn::Waiting) };
        let Some(bind) = opened.binds.get(frame.slot()) else {
            ring.refuse(Fallback::ImportFailed);
            return Err(Fallback::ImportFailed);
        };
        let Some(opaque) = &mut self.opaque else { return Ok(Drawn::Idle) };
        let Some(target) = target(frame.width(), frame.height()) else {
            // The texture goes back to the capture side unread.
            release(opened, frame.ready())?;
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
fn open(device: &wgpu::Device, queue: &wgpu::Queue, ring: &crate::ring::SharedRing, opaque: &Opaque) -> Result<Opened, Fallback> {
    let raw = match crate::d3d12_ffi::import(device, ring) {
        Err(Fallback::UnsupportedBackend) => Raw::Vulkan(Box::new(crate::vulkan_ffi::import(device, queue, ring)?)),
        imported => Raw::Dx12(imported?),
    };
    let textures = match &raw {
        Raw::Dx12(r) => &r.textures,
        Raw::Vulkan(r) => &r.textures,
    };
    let binds = textures.iter().map(|t| opaque.bind(device, &t.create_view(&wgpu::TextureViewDescriptor::default()))).collect();
    Ok(Opened { ring: ring.id(), binds, raw })
}

#[cfg(not(windows))]
fn open(_device: &wgpu::Device, _queue: &wgpu::Queue, _ring: &crate::ring::SharedRing, _opaque: &Opaque) -> Result<Opened, Fallback> {
    Err(Fallback::Unsupported)
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

#[cfg(not(windows))]
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

#[cfg(not(windows))]
fn hand_back(_opened: &Opened, _device: &wgpu::Device, _slot: usize) -> Option<wgpu::CommandBuffer> {
    None
}

#[cfg(windows)]
fn completed(opened: &Opened) -> u64 {
    match &opened.raw {
        Raw::Dx12(r) => r.completed(),
        Raw::Vulkan(r) => r.completed(),
    }
}

#[cfg(not(windows))]
fn completed(_opened: &Opened) -> u64 {
    0
}

#[cfg(windows)]
fn poll(opened: &Opened) {
    if let Raw::Vulkan(r) = &opened.raw {
        r.poll();
    }
}

#[cfg(not(windows))]
fn poll(_opened: &Opened) {}

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
