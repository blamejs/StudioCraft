//! A 1080p and a 4K BGRA picture into a compositor's source texture: through `write_texture` from
//! CPU memory, as a CPU capture is uploaded, and through the opaque copy from a texture already on
//! the GPU, as a shared capture is drawn. Each iteration waits until the GPU has finished. On
//! Windows each case runs on a Direct3D 12 and on a Vulkan device, the two backends a shared
//! capture is opened on, with the backend at the end of its name. On a machine without a graphics
//! adapter nothing runs.

use craft_gpu_interop::Opaque;
use criterion::{Criterion, criterion_group, criterion_main};

/// The backends to measure on, each with the suffix of its benches' names.
#[cfg(windows)]
const BACKENDS: [(&str, wgpu::Backends); 2] = [(" dx12", wgpu::Backends::DX12), (" vulkan", wgpu::Backends::VULKAN)];
#[cfg(not(windows))]
const BACKENDS: [(&str, wgpu::Backends); 1] = [("", wgpu::Backends::PRIMARY)];

fn device(backends: wgpu::Backends) -> Option<(wgpu::Device, wgpu::Queue)> {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = backends;
    let instance = wgpu::Instance::new(desc);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
    let info = adapter.get_info();
    eprintln!("adapter: {} ({:?}, {:?})", info.name, info.backend, info.device_type);
    let desc = wgpu::DeviceDescriptor { required_limits: adapter.limits(), ..Default::default() };
    pollster::block_on(adapter.request_device(&desc)).ok()
}

fn texture(device: &wgpu::Device, w: u32, h: u32, usage: wgpu::TextureUsages) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage,
        view_formats: &[],
    })
}

fn bench(c: &mut Criterion) {
    for (suffix, backends) in BACKENDS {
        match device(backends) {
            Some((device, queue)) => bench_on(c, &device, &queue, suffix),
            None => eprintln!("no graphics adapter on {backends:?}: nothing to measure there"),
        }
    }
}

fn bench_on(c: &mut Criterion, device: &wgpu::Device, queue: &wgpu::Queue, suffix: &str) {
    let mut opaque = Opaque::new(device);
    for (size, w, h) in [("1080p", 1920u32, 1080u32), ("4k", 3840, 2160)] {
        let label = format!("{size}{suffix}");
        let pixels = vec![200u8; (w * h * 4) as usize];
        let dst =
            texture(device, w, h, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::RENDER_ATTACHMENT);
        let src = texture(device, w, h, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
        let extent = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
        let layout = wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) };
        queue.write_texture(src.as_image_copy(), &pixels, layout, extent);
        c.bench_function(&format!("write_texture upload {label}"), |b| {
            b.iter(|| {
                queue.write_texture(dst.as_image_copy(), &pixels, layout, extent);
                queue.submit([]);
                let _ = device.poll(wgpu::PollType::wait_indefinitely());
            })
        });
        let bind = opaque.bind(device, &src.create_view(&wgpu::TextureViewDescriptor::default()));
        let target = dst.create_view(&wgpu::TextureViewDescriptor::default());
        c.bench_function(&format!("opaque GPU copy {label}"), |b| {
            b.iter(|| {
                let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                opaque.draw(device, &mut enc, &bind, &target, wgpu::TextureFormat::Bgra8Unorm);
                queue.submit([enc.finish()]);
                let _ = device.poll(wgpu::PollType::wait_indefinitely());
            })
        });
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
