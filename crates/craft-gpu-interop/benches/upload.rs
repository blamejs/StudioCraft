//! A 1080p and a 4K BGRA picture into a compositor's source texture: through `write_texture` from
//! CPU memory, as a CPU capture is uploaded, and through the opaque copy from a texture already on
//! the GPU, as a shared capture is drawn. The same two cases run for a 1280 × 720 and a
//! 2560 × 1440 window picture (`window 720p`, `window 1440p`), the sizes a window capture often
//! has, with the GPU copy keeping the picture's alpha (`alpha GPU copy`). Each iteration waits until the GPU has finished. On Windows each case runs on a Direct3D
//! 12 and on a Vulkan device, the two backends a shared capture is opened on, with the backend at
//! the end of its name. On a machine without a graphics adapter nothing runs.
//!
//! The other direction hands an NV12 program frame (an R8 luma and an RG8 chroma plane) to an
//! encoder: read back into a mapped buffer, as the compositor does without an export ring, and
//! copied into an export ring's texture, which runs only where [`Exporter::open`] succeeds
//! (Windows on Direct3D 12, or Vulkan with the shared-memory extension; Linux on Vulkan with the
//! DMA-BUF and DRM format modifier extensions, where the ring's images are DMA-BUFs a VA-API
//! encoder imports). Comparing `NV12 read-back` with `NV12 export ring copy` at a size gives what
//! the GPU path saves per frame before the encoder. A copy, submission or release that fails during
//! the export case stops the run.
//!
//! On Linux the display sizes also run `DMA-BUF import`: a linear BGRX DMA-BUF made over ordinary
//! memory through `/dev/udmabuf`, handed to the importer as a screen-cast buffer and drawn into the
//! source texture, on a Vulkan device with the DMA-BUF extensions. It does not run, and says why,
//! where the device cannot import a linear buffer or `/dev/udmabuf` cannot be opened.

#![allow(clippy::panic)]

use craft_gpu_interop::{Exporter, Opaque};
use criterion::{Criterion, criterion_group, criterion_main};

/// The backends to measure on, each with the suffix of its benches' names.
#[cfg(windows)]
const BACKENDS: [(&str, wgpu::Backends); 2] = [(" dx12", wgpu::Backends::DX12), (" vulkan", wgpu::Backends::VULKAN)];
#[cfg(not(windows))]
const BACKENDS: [(&str, wgpu::Backends); 1] = [("", wgpu::Backends::PRIMARY)];

/// The features an export ring needs on the measured device, where the adapter has them.
#[cfg(target_os = "linux")]
const EXPORT_FEATURES: wgpu::Features = wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF;
/// Elsewhere the Vulkan export opens its ring only on a device with the shared-memory feature.
#[cfg(not(target_os = "linux"))]
const EXPORT_FEATURES: wgpu::Features = wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32;

fn device(backends: wgpu::Backends) -> Option<(wgpu::Device, wgpu::Queue)> {
    device_with(backends, EXPORT_FEATURES)
}

/// A device on `backends` with those of `features` its adapter has.
fn device_with(backends: wgpu::Backends, features: wgpu::Features) -> Option<(wgpu::Device, wgpu::Queue)> {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = backends;
    let instance = wgpu::Instance::new(desc);
    let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).ok()?;
    let info = adapter.get_info();
    eprintln!("adapter: {} ({:?}, {:?})", info.name, info.backend, info.device_type);
    let desc = wgpu::DeviceDescriptor { required_features: adapter.features() & features, required_limits: adapter.limits(), ..Default::default() };
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
    #[cfg(target_os = "linux")]
    bench_dmabuf(c);
}

/// A linear BGRX DMA-BUF of each display size into a source texture, as a Linux display capture's
/// buffers are drawn.
#[cfg(target_os = "linux")]
fn bench_dmabuf(c: &mut Criterion) {
    use craft_gpu_interop::{DmabufFormat, DmabufLayout, DmabufStream, Importer, MOD_LINEAR};
    use std::os::fd::AsRawFd;
    let Some((device, queue)) = device_with(wgpu::Backends::VULKAN, wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF) else {
        return eprintln!("no Vulkan adapter: no DMA-BUF import to measure");
    };
    craft_gpu_interop::publish_import_caps(&device);
    let Some(caps) = craft_gpu_interop::dmabuf_caps().filter(|caps| caps.supports(DmabufFormat::Bgrx, MOD_LINEAR)) else {
        return eprintln!("the Vulkan device cannot import a linear BGRX DMA-BUF: no DMA-BUF import to measure");
    };
    for (size, w, h) in [("1080p", 1920u32, 1080u32), ("4k", 3840, 2160)] {
        let stride = w * 4;
        // A whole number of 4 KiB pages.
        let len = (u64::from(stride) * u64::from(h)).div_ceil(4096) * 4096;
        let buffer = match craft_gpu_interop::dmabuf_in_memory(len) {
            Ok(fd) => fd,
            Err(e) => return eprintln!("no DMA-BUF in memory for {size} ({e}): no DMA-BUF import to measure"),
        };
        let mut stream = DmabufStream::new();
        stream.set_format(Some((w, h, DmabufLayout { format: DmabufFormat::Bgrx, modifier: MOD_LINEAR }, caps.device)));
        let dst =
            texture(&device, w, h, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::RENDER_ATTACHMENT);
        let target = dst.create_view(&wgpu::TextureViewDescriptor::default());
        let mut importer = Importer::new();
        c.bench_function(&format!("DMA-BUF import {size}"), |b| {
            b.iter(|| {
                let Ok(Some(frame)) = stream.frame(buffer.as_raw_fd(), 0, stride) else { return };
                importer.offer(frame);
                let drawn = importer.draw(&device, &queue, wgpu::TextureFormat::Bgra8Unorm, |_, _| Some(target.clone()));
                std::hint::black_box(drawn.ok());
                let _ = device.poll(wgpu::PollType::wait_indefinitely());
            })
        });
    }
}

fn bench_on(c: &mut Criterion, device: &wgpu::Device, queue: &wgpu::Queue, suffix: &str) {
    let mut opaque = Opaque::new(device);
    // The program frames an encoder takes are the canvas's size, so only the display sizes have
    // the NV12 cases. A window's picture is drawn keeping its alpha, as a window capture draws it.
    for (size, w, h, display) in
        [("1080p", 1920u32, 1080u32, true), ("4k", 3840, 2160, true), ("window 720p", 1280, 720, false), ("window 1440p", 2560, 1440, false)]
    {
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
        let name = if display { "opaque GPU copy" } else { "alpha GPU copy" };
        c.bench_function(&format!("{name} {label}"), |b| {
            b.iter(|| {
                let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
                if display {
                    opaque.draw(device, &mut enc, &bind, &target, wgpu::TextureFormat::Bgra8Unorm);
                } else {
                    opaque.draw_keeping_alpha(device, &mut enc, &bind, &target, wgpu::TextureFormat::Bgra8Unorm);
                }
                queue.submit([enc.finish()]);
                let _ = device.poll(wgpu::PollType::wait_indefinitely());
            })
        });
        if display {
            bench_encode(c, device, queue, &label, w, h);
        }
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
    // Both planes have rows of `w` bytes. A texture-to-buffer copy needs each row in the buffer to
    // start on a multiple of 256 bytes, so a 1920-byte row is padded to 2048.
    let stride = w.next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
    let luma = u64::from(stride) * u64::from(h);
    let buffer = device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: luma + u64::from(stride) * u64::from(h / 2),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let copy = |enc: &mut wgpu::CommandEncoder, texture: &wgpu::Texture, offset: u64, rows: u32| {
        enc.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset, bytes_per_row: Some(stride), rows_per_image: Some(rows) },
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
            // A failed mapping, wait or read fails the case instead of counting as a read-back.
            let (sent, mapped) = std::sync::mpsc::channel();
            buffer.map_async(wgpu::MapMode::Read, .., move |r| {
                let _ = sent.send(r);
            });
            if let Err(e) = device.poll(wgpu::PollType::wait_indefinitely()) {
                panic!("the NV12 read-back for {label} did not finish: {e:?}");
            }
            match mapped.try_recv() {
                Ok(Ok(())) => {}
                other => panic!("the NV12 read-back for {label} was not mapped: {other:?}"),
            }
            {
                let view = buffer.get_mapped_range(..).unwrap_or_else(|e| panic!("the NV12 read-back for {label} could not be read: {e:?}"));
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
            // A failed copy, submission or release fails the case.
            let (commands, ticket) = match exporter.copy(device, &planes, std::time::Instant::now()) {
                Ok(Some(copy)) => copy,
                Ok(None) => panic!("no free export texture for {label}"),
                Err(reason) => panic!("the export copy for {label} failed: {reason}"),
            };
            queue.submit([enc.finish(), commands]);
            let frame = exporter.submitted(ticket).unwrap_or_else(|reason| panic!("the export submission for {label} failed: {reason}"));
            let _ = device.poll(wgpu::PollType::wait_indefinitely());
            // The encoder's side hands each texture straight back, and the next copy starts once
            // the released fence has the value, so every iteration times a copy.
            exporter.ring().release(frame.ready());
            while exporter.ring().released() < frame.ready() {
                if let Some(reason) = exporter.ring().fallback() {
                    panic!("the export release for {label} failed: {reason}");
                }
                std::hint::spin_loop();
            }
        })
    });
}

criterion_group!(benches, bench);
criterion_main!(benches);
