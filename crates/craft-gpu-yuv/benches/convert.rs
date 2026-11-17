//! GPU conversion of a 1080p and a 4K picture, from the passes to a mapped read-back buffer. On a
//! machine without a graphics adapter the GPU cases are left out; the CPU reference at 1080p
//! always runs.

use craft_gpu_yuv::{Conversion, Converter, Format, Matrix, Range, cpu};
use criterion::{Criterion, criterion_group, criterion_main};

fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
    let desc = wgpu::DeviceDescriptor { required_limits: adapter.limits(), ..Default::default() };
    pollster::block_on(adapter.request_device(&desc)).ok()
}

fn bench(c: &mut Criterion) {
    let conversion = Conversion::new(Format::Nv12, Matrix::Bt709, Range::Limited);
    let rgba = vec![128u8; 1920 * 1080 * 4];
    c.bench_function("cpu nv12 1080p", |b| b.iter(|| cpu::convert(&rgba, 1920, 1080, conversion, 1920, 1080)));
    let Some((device, queue)) = device() else {
        eprintln!("no graphics adapter: the GPU cases are left out");
        return;
    };
    for (name, w, h, format) in [
        ("gpu nv12 1080p", 1920, 1080, Format::Nv12),
        ("gpu p010 1080p", 1920, 1080, Format::P010),
        ("gpu nv12 4k", 3840, 2160, Format::Nv12),
        ("gpu nv12 4k to 1080p", 1920, 1080, Format::Nv12),
    ] {
        let (sw, sh) = if name.contains("4k") { (3840, 2160) } else { (w, h) };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: sw, height: sh, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let converter = Converter::new(&device, Conversion { format, ..conversion });
        let source = converter.bind(&device, &texture.create_view(&wgpu::TextureViewDescriptor::default()));
        let Ok(planes) = converter.planes(&device, w, h) else { continue };
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: planes.layout().buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        c.bench_function(name, |b| {
            b.iter(|| {
                let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                let _ = converter.draw(&mut enc, &source, &planes);
                let _ = planes.copy_to_buffer(&mut enc, &buffer, 0);
                queue.submit([enc.finish()]);
                buffer.map_async(wgpu::MapMode::Read, .., |_| {});
                let _ = device.poll(wgpu::PollType::wait_indefinitely());
                buffer.unmap();
            })
        });
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
