//! A 1080p and a 4K BGRA picture into a compositor's source texture: through `write_texture` from
//! CPU memory, as a CPU capture is uploaded, and through the opaque copy from a texture already on
//! the GPU, as a shared capture is drawn. Each iteration waits until the GPU has finished. On
//! Windows each case runs on a Direct3D 12 and on a Vulkan device, the two backends a shared
//! capture is opened on, with the backend at the end of its name. On a machine without a graphics
//! adapter nothing runs.
//!
//! The other direction hands an NV12 program frame (an R8 luma and an RG8 chroma plane) to an
//! encoder: read back into a mapped buffer, as the compositor does without an export ring, and
//! copied into an export ring's texture, which runs only where [`Exporter::open`] succeeds
//! (Windows on Direct3D 12, or Vulkan with the shared-memory extension).

use craft_gpu_interop::{Exporter, Opaque};
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
        bench_encode(c, device, queue, &label, w, h);
    }
}

fn plane(device: &wgpu::Device, format: wgpu::TextureFormat, w: u32, h: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

/// A `w × h` NV12 frame to an encoder: read back through memory, and copied into an export ring.
fn bench_encode(c: &mut Criterion, device: &wgpu::Device, queue: &wgpu::Queue, label: &str, w: u32, h: u32) {
    let planes = [plane(device, wgpu::TextureFormat::R8Unorm, w, h), plane(device, wgpu::TextureFormat::Rg8Unorm, w / 2, h / 2)];
    // Rows of 1920 and 3840 bytes are multiples of 256, as a texture-to-buffer copy needs.
    let luma = u64::from(w) * u64::from(h);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: luma + luma / 2,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let copy = |enc: &mut wgpu::CommandEncoder, texture: &wgpu::Texture, offset: u64, rows: u32| {
        enc.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset, bytes_per_row: Some(w), rows_per_image: Some(rows) },
            },
            texture.size(),
        );
    };
    c.bench_function(&format!("NV12 read-back {label}"), |b| {
        b.iter(|| {
            let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            copy(&mut enc, &planes[0], 0, h);
            copy(&mut enc, &planes[1], luma, h / 2);
            queue.submit([enc.finish()]);
            buffer.map_async(wgpu::MapMode::Read, .., |_| {});
            let _ = device.poll(wgpu::PollType::wait_indefinitely());
            if let Ok(view) = buffer.get_mapped_range(..) {
                std::hint::black_box(view.len());
            }
            buffer.unmap();
        })
    });
    let mut exporter = match Exporter::open(device, queue, w, h) {
        Ok(e) => e,
        Err(reason) => return eprintln!("no export ring for {label}: {reason}"),
    };
    exporter.reserve(device, &planes);
    c.bench_function(&format!("NV12 export ring copy {label}"), |b| {
        b.iter(|| {
            let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            exporter.prepare(device, &mut enc, &planes);
            let Ok(Some((commands, ticket))) = exporter.copy(device, &planes, std::time::Instant::now()) else { return };
            queue.submit([enc.finish(), commands]);
            let frame = exporter.submitted(ticket);
            let _ = device.poll(wgpu::PollType::wait_indefinitely());
            // The encoder's side hands each texture straight back, and the next copy starts once
            // the released fence has the value, so every iteration times a copy.
            if let Ok(frame) = frame {
                exporter.ring().release(frame.ready());
                while exporter.ring().released() < frame.ready() && exporter.ring().fallback().is_none() {
                    std::hint::spin_loop();
                }
            }
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
