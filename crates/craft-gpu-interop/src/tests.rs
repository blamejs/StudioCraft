use std::time::{Duration, Instant};

use super::*;

fn frame(ring: &std::sync::Arc<SharedRing>, slot: usize, ready: u64) -> SharedFrame {
    SharedFrame::new(std::sync::Arc::clone(ring), slot, ready).unwrap()
}

#[test]
fn free_textures_are_written_oldest_first() {
    let t0 = Instant::now();
    let ring = SharedRing::detached(1, 1);
    let mut slots = SlotTable::default();
    let mut order = Vec::new();
    for _ in 0..SLOTS {
        let s = slots.pick(&ring, 0, t0).unwrap();
        assert!(!order.contains(&s), "a texture handed out is not picked again");
        order.push(s);
        slots.fill(&ring, s, t0).unwrap();
    }
    assert_eq!(slots.last(), 3);
    assert_eq!(slots.pick(&ring, 0, t0), None, "every texture is waiting for the compositor");
    // The compositor read frame 1: its texture is the one written again.
    assert_eq!(slots.pick(&ring, 1, t0), Some(order[0]));
    // Frame 3 read means frames 1 and 2 were passed over; all are free, the oldest first.
    assert_eq!(slots.pick(&ring, 3, t0), Some(order[0]));
}

#[test]
fn a_texture_the_compositor_never_releases_is_taken_back() {
    let t0 = Instant::now();
    let ring = SharedRing::detached(1, 1);
    let mut slots = SlotTable::default();
    for _ in 0..SLOTS {
        let s = slots.pick(&ring, 0, t0).unwrap();
        slots.fill(&ring, s, t0).unwrap();
    }
    assert_eq!(slots.pick(&ring, 0, t0 + RECLAIM - Duration::from_millis(1)), None);
    let s = slots.pick(&ring, 0, t0 + RECLAIM).unwrap();
    assert_eq!(slots.fill(&ring, s, t0 + RECLAIM), Some(4));
    // A clock that went back does not free a texture.
    let later = t0 + RECLAIM * 4;
    let mut slots = SlotTable::default();
    for _ in 0..SLOTS {
        let s = slots.pick(&ring, 0, later).unwrap();
        slots.fill(&ring, s, later).unwrap();
    }
    assert_eq!(slots.pick(&ring, 0, t0), None);
}

#[test]
fn a_texture_the_compositor_started_to_read_waits_for_the_released_fence() {
    let t0 = Instant::now();
    let ring = SharedRing::detached(1, 1);
    let mut slots = SlotTable::default();
    let mut frames = Vec::new();
    for _ in 0..SLOTS {
        let s = slots.pick(&ring, 0, t0).unwrap();
        frames.push(frame(&ring, s, slots.fill(&ring, s, t0).unwrap()));
    }
    // The compositor submits a draw of frame 1 that is still running after RECLAIM.
    assert!(frames[0].start_reading());
    assert!(!frames[0].start_reading(), "a frame is read once");
    let late = t0 + RECLAIM * 2;
    let s = slots.pick(&ring, 0, late).unwrap();
    assert_ne!(s, frames[0].slot(), "the texture being read is not written");
    assert_eq!(s, frames[1].slot(), "the oldest texture nobody reads is taken back");
    // A frame taken back is not drawn: its texture may hold another picture.
    assert!(!frames[1].start_reading());
    assert!(!frames[2].start_reading(), "a texture past RECLAIM that was passed over is taken back too");
    slots.fill(&ring, s, late).unwrap();
    assert_eq!(slots.pick(&ring, 0, late), Some(frames[2].slot()));
    // Once the released fence passes frame 1, its texture is free again.
    assert_eq!(slots.pick(&ring, 1, late), Some(frames[0].slot()));
}

#[test]
fn a_slot_outside_the_ring_is_refused() {
    let ring = SharedRing::detached(4, 2);
    let mut slots = SlotTable::default();
    assert_eq!(slots.fill(&ring, SLOTS, Instant::now()), None);
    assert_eq!(slots.last(), 0, "a refused fill hands out no value");
    assert!(SharedFrame::new(ring.clone(), SLOTS, 1).is_none());
    assert!(SharedFrame::new(ring.clone(), usize::MAX, 1).is_none());
    let f = frame(&ring, SLOTS - 1, 9);
    assert_eq!((f.slot(), f.ready(), f.width(), f.height()), (SLOTS - 1, 9, 4, 2));
}

#[test]
fn a_late_copy_leaves_the_frame_waiting_and_the_caller_goes_on() {
    let ring = SharedRing::detached(1, 1);
    let mut handoff = Handoff::default();
    assert!(handoff.take_ready(u64::MAX).is_none(), "nothing offered");
    handoff.offer(frame(&ring, 0, 5));
    // The capture side's fence is behind: each call returns at once and the frame stays.
    for completed in [0, 3, 4] {
        assert!(handoff.take_ready(completed).is_none());
        assert_eq!(handoff.pending().map(SharedFrame::ready), Some(5));
    }
    // A newer frame takes the waiting one's place.
    handoff.offer(frame(&ring, 1, 6));
    assert!(handoff.take_ready(4).is_none());
    assert_eq!(handoff.pending().map(SharedFrame::ready), Some(6));
    let taken = handoff.take_ready(6).unwrap();
    assert_eq!((taken.slot(), taken.ready()), (1, 6));
    assert!(handoff.pending().is_none(), "a frame is taken once");
    assert!(handoff.take_ready(u64::MAX).is_none(), "the frame it replaced is dropped with it");
}

#[test]
fn a_finished_frame_is_drawn_while_the_newer_one_is_copied() {
    let ring = SharedRing::detached(1, 1);
    let mut handoff = Handoff::default();
    handoff.offer(frame(&ring, 0, 5));
    handoff.offer(frame(&ring, 1, 6));
    // Frame 5's copy is done and frame 6's is not: frame 5 is taken and frame 6 keeps waiting.
    assert_eq!(handoff.take_ready(5).map(|f| f.ready()), Some(5));
    assert_eq!(handoff.pending().map(SharedFrame::ready), Some(6));
    assert!(handoff.take_ready(5).is_none(), "the older frame is taken once");
    // Only the frame the newest one replaced is kept: frame 6 is dropped.
    handoff.offer(frame(&ring, 2, 7));
    handoff.offer(frame(&ring, 0, 8));
    assert!(handoff.take_ready(6).is_none());
    assert_eq!(handoff.take_ready(7).map(|f| f.ready()), Some(7));
    // A frame from another ring is not kept.
    let other = SharedRing::detached(1, 1);
    handoff.offer(frame(&other, 0, 9));
    assert!(handoff.take_ready(8).is_none());
    assert_eq!(handoff.take_ready(9).map(|f| f.ready()), Some(9));
}

#[test]
fn the_first_fallback_reason_is_kept() {
    let ring = SharedRing::detached(1, 1);
    assert_eq!(ring.fallback(), None);
    ring.refuse(Fallback::UnsupportedBackend);
    ring.refuse(Fallback::OtherAdapter);
    assert_eq!(ring.fallback(), Some(Fallback::UnsupportedBackend));
    for f in Fallback::ALL {
        assert_eq!(Fallback::from_code(f.code()), Some(f));
        assert!(!f.to_string().is_empty());
    }
    assert_eq!(Fallback::from_code(0), None);
    assert_eq!(Fallback::from_code(200), None);
    assert_ne!(SharedRing::detached(1, 1).id(), ring.id(), "every ring has its own id");
}

#[test]
fn an_import_needs_direct3d_12_on_the_display_adapter() {
    let a = AdapterId::from_parts(7, 0);
    let b = AdapterId::from_parts(7, 1);
    assert_eq!(check_import(Backend::Dx12, a, Some(a)), Ok(()));
    assert_eq!(check_import(Backend::Other, a, Some(a)), Err(Fallback::UnsupportedBackend));
    assert_eq!(check_import(Backend::Dx12, a, Some(b)), Err(Fallback::OtherAdapter));
    assert_eq!(check_import(Backend::Dx12, a, None), Err(Fallback::OtherAdapter));
}

#[test]
fn a_vulkan_import_needs_the_display_adapter_the_external_memory_extension_and_timeline_semaphores() {
    let a = AdapterId::from_parts(7, 0);
    let b = AdapterId::from_parts(8, 0);
    let with = Backend::Vulkan { external_memory: true, timeline: true };
    let without = Backend::Vulkan { external_memory: false, timeline: true };
    let no_timeline = Backend::Vulkan { external_memory: true, timeline: false };
    assert_eq!(check_import(with, a, Some(a)), Ok(()), "a Vulkan compositor on the display's adapter opens the ring");
    assert_eq!(check_import(without, a, Some(a)), Err(Fallback::MissingExtension));
    assert_eq!(check_import(no_timeline, a, Some(a)), Err(Fallback::MissingExtension));
    assert_eq!(check_import(Backend::Vulkan { external_memory: false, timeline: false }, a, Some(a)), Err(Fallback::MissingExtension));
    // The adapter is checked before the extension: either way the ring cannot be opened.
    assert_eq!(check_import(with, a, Some(b)), Err(Fallback::OtherAdapter));
    assert_eq!(check_import(without, a, Some(b)), Err(Fallback::OtherAdapter));
    // A driver that does not report its adapter counts as another adapter.
    assert_eq!(check_import(with, a, None), Err(Fallback::OtherAdapter));
    assert_eq!(check_import(Backend::Other, a, None), Err(Fallback::UnsupportedBackend));
}

#[test]
fn a_failed_open_destroys_the_resources_opened_before_it() {
    let mut destroyed = Vec::new();
    let result = crate::fallback::open_all(&[1, 2, 3], |h| if h == 3 { Err(Fallback::ImportFailed) } else { Ok(h * 10) }, |r| destroyed.push(r));
    assert_eq!(result, Err(Fallback::ImportFailed));
    assert_eq!(destroyed, [10, 20], "both textures opened before the failure are destroyed");

    let mut destroyed = Vec::new();
    let result = crate::fallback::open_all(&[1, 2, 3], |h| Ok::<_, Fallback>(h * 10), |r| destroyed.push(r));
    assert_eq!(result, Ok(vec![10, 20, 30]));
    assert!(destroyed.is_empty(), "nothing is destroyed when every open succeeds");

    let result = crate::fallback::open_all(&[1], |_| Err::<u32, _>(Fallback::ImportFailed), |_| panic!("nothing was opened"));
    assert_eq!(result, Err(Fallback::ImportFailed));
}

#[test]
fn an_adapter_id_keeps_both_parts() {
    assert_eq!(AdapterId::from_parts(1, 2).0, (2 << 32) | 1);
    assert_eq!(AdapterId::from_parts(u32::MAX, -1).0, u64::MAX);
    assert_ne!(AdapterId::from_parts(0, -1), AdapterId::from_parts(0, 1));
    for (low, high) in [(0, 0), (1, 2), (u32::MAX, -1), (0x1234_5678, i32::MIN)] {
        assert_eq!(AdapterId::from_parts(low, high).parts(), (low, high));
    }
}

#[test]
fn an_adapter_id_from_vulkan_is_the_one_windows_reports() {
    // Vulkan's deviceLUID is the Windows LUID's bytes: the low part, then the high part.
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&0x0000_d2a1u32.to_le_bytes());
    bytes[4..].copy_from_slice(&(-2i32).to_le_bytes());
    assert_eq!(AdapterId::from_luid_bytes(bytes), AdapterId::from_parts(0x0000_d2a1, -2));
    assert_eq!(AdapterId::from_luid_bytes([0; 8]), AdapterId(0));
}

#[cfg(not(windows))]
#[test]
fn elsewhere_than_windows_the_capture_reports_the_fallback() {
    assert_eq!(Duplication::open("display").err(), Some(CaptureError::Fallback(Fallback::Unsupported)));
}

#[cfg(feature = "wgpu")]
mod gpu {
    use super::*;

    fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())) else {
            eprintln!("no graphics adapter: this test is skipped");
            return None;
        };
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
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

    /// Read a 64 x 1 BGRA texture back (64 texels make a 256-byte row).
    fn read(device: &wgpu::Device, queue: &wgpu::Queue, tex: &wgpu::Texture) -> Vec<u8> {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 256,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        enc.copy_texture_to_buffer(
            tex.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256), rows_per_image: Some(1) },
            },
            wgpu::Extent3d { width: 64, height: 1, depth_or_array_layers: 1 },
        );
        queue.submit([enc.finish()]);
        buffer.map_async(wgpu::MapMode::Read, .., |r| r.unwrap());
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        buffer.get_mapped_range(..).unwrap().to_vec()
    }

    #[test]
    fn the_opaque_copy_keeps_the_color_and_sets_alpha() {
        let Some((device, queue)) = device() else { return };
        let src = texture(&device, 64, 1, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
        let dst = texture(&device, 64, 1, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let mut texels = Vec::new();
        for i in 0..64u8 {
            texels.extend([i, i.wrapping_mul(3), 255 - i, i % 2 * 255]);
        }
        queue.write_texture(
            src.as_image_copy(),
            &texels,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256), rows_per_image: Some(1) },
            wgpu::Extent3d { width: 64, height: 1, depth_or_array_layers: 1 },
        );
        let mut opaque = Opaque::new(&device);
        let bind = opaque.bind(&device, &src.create_view(&Default::default()));
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        opaque.draw(&device, &mut enc, &bind, &dst.create_view(&Default::default()), wgpu::TextureFormat::Bgra8Unorm);
        queue.submit([enc.finish()]);
        let out = read(&device, &queue, &dst);
        for (i, (got, want)) in out.chunks(4).zip(texels.chunks(4)).enumerate() {
            assert_eq!(&got[..3], &want[..3], "texel {i} keeps its color");
            assert_eq!(got[3], 255, "texel {i} is opaque");
        }
    }

    #[test]
    fn a_ring_the_device_cannot_open_records_why_and_drops_the_frame() {
        let Some((device, queue)) = device() else { return };
        let ring = SharedRing::detached(64, 1);
        let format = wgpu::TextureFormat::Bgra8Unorm;
        let asked = std::cell::Cell::new(0);
        let target = || {
            asked.set(asked.get() + 1);
            Some(texture(&device, 64, 1, wgpu::TextureUsages::RENDER_ATTACHMENT).create_view(&Default::default()))
        };
        let mut importer = Importer::new();
        importer.poll();
        assert_eq!(importer.draw(&device, &queue, format, |_, _| target()), Ok(Drawn::Idle));
        importer.offer(frame(&ring, 0, 1));
        let reason = importer.draw(&device, &queue, format, |_, _| target()).unwrap_err();
        assert_eq!(ring.fallback(), Some(reason));
        assert!(importer.pending().is_none());
        // A later frame of the same ring is dropped with the recorded reason, without another
        // attempt to open it.
        importer.offer(frame(&ring, 1, 2));
        assert_eq!(importer.draw(&device, &queue, format, |_, _| target()), Err(reason));
        assert_eq!(importer.draw(&device, &queue, format, |_, _| target()), Ok(Drawn::Idle));
        assert_eq!(asked.get(), 0, "no target is made for a frame that is not drawn");
    }

    /// A device on wgpu's Vulkan backend, opened with those of `features` the adapter has.
    #[cfg(windows)]
    fn vulkan(features: wgpu::Features) -> Option<(wgpu::Device, wgpu::Queue)> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(desc);
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())) else {
            eprintln!("no Vulkan adapter: this test is skipped");
            return None;
        };
        let desc = wgpu::DeviceDescriptor { required_features: features & adapter.features(), ..Default::default() };
        pollster::block_on(adapter.request_device(&desc)).ok()
    }

    #[cfg(windows)]
    #[test]
    fn a_vulkan_compositor_opens_rings_of_its_own_adapter() {
        let Some((device, queue)) = vulkan(wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32) else { return };
        let (Some(own), timeline) = crate::vulkan_ffi::adapter(&device).expect("a Vulkan device") else {
            eprintln!("the Vulkan driver does not report its adapter: this test is skipped");
            return;
        };
        // A ring of another adapter is refused for the adapter, not for the backend.
        let other = SharedRing::detached_on(64, 1, AdapterId(own.0 ^ 1));
        let mut importer = Importer::new();
        importer.offer(frame(&other, 0, 1));
        assert_eq!(importer.draw(&device, &queue, wgpu::TextureFormat::Bgra8Unorm, |_, _| None), Err(Fallback::OtherAdapter));
        assert_eq!(other.fallback(), Some(Fallback::OtherAdapter));
        // A ring of its own adapter gets past both checks; this one has no shared handles to open.
        let ring = SharedRing::detached_on(64, 1, own);
        let expected = if timeline && device.features().contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32) {
            Fallback::ImportFailed
        } else {
            Fallback::MissingExtension
        };
        assert_eq!(crate::vulkan_ffi::import(&device, &queue, &ring).err(), Some(expected));
        // A device opened without the extension cannot open the textures.
        let Some((plain, plain_queue)) = vulkan(wgpu::Features::empty()) else { return };
        assert_eq!(crate::vulkan_ffi::import(&plain, &plain_queue, &ring).err(), Some(Fallback::MissingExtension));
    }

    #[cfg(windows)]
    #[test]
    fn a_direct3d_12_device_is_not_opened_as_vulkan() {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::DX12;
        let instance = wgpu::Instance::new(desc);
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())) else {
            eprintln!("no Direct3D 12 adapter: this test is skipped");
            return;
        };
        let Ok((device, queue)) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())) else { return };
        assert!(crate::vulkan_ffi::adapter(&device).is_none());
        let ring = SharedRing::detached(64, 1);
        assert_eq!(crate::vulkan_ffi::import(&device, &queue, &ring).err(), Some(Fallback::UnsupportedBackend));
    }
}
