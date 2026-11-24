//! The compositor side on Windows when the compositor's wgpu device runs on Vulkan: a ring's shared
//! textures opened as Vulkan images through `VK_KHR_external_memory_win32`, and its two fences
//! opened on a Direct3D 12 device of the same adapter. Every wgpu-hal, Vulkan and Direct3D 12 call
//! of this import is here.
//!
//! wgpu opens its Vulkan devices without `VK_KHR_external_semaphore_win32`, and the fences are not
//! imported as Vulkan semaphores. The importer reads the ready fence from the CPU. A submission after
//! a draw signals a timeline semaphore of the importer's own with the frame's value, and the
//! importer copies the semaphore's value to the released fence from the CPU on a later call. Each
//! draw takes the texture's ownership from the external queue family before it reads the texture,
//! and hands it back after.

use std::sync::atomic::{AtomicU64, Ordering};

use ash::vk;
use windows::Win32::Foundation::LUID;
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::{D3D12CreateDevice, ID3D12Device, ID3D12Fence};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};

use crate::fallback::{Backend, Fallback, check_import};
use crate::ring::{AdapterId, SharedRing};

/// A ring opened on one Vulkan device: its textures as wgpu textures, and its two fences.
pub(crate) struct Imported {
    pub(crate) textures: Vec<wgpu::Texture>,
    images: Vec<vk::Image>,
    family: u32,
    device: ash::Device,
    queue: wgpu::Queue,
    /// Signaled by the compositor's queue with a frame's value once the draw that read it is done.
    read: vk::Semaphore,
    ready: ID3D12Fence,
    released: ID3D12Fence,
    /// The newest value queued on `read`.
    queued: AtomicU64,
    /// The newest value on `read` that went out with a submission, apart from `queued`.
    submitted: AtomicU64,
    /// The newest value set on `released`.
    signaled: AtomicU64,
}

/// What the device's driver reports: its adapter (`None` when it does not report one), and whether
/// it has timeline semaphores. `None` when the device does not run on Vulkan.
pub(crate) fn adapter(device: &wgpu::Device) -> Option<(Option<AdapterId>, bool)> {
    // SAFETY: the guard is only used to read the physical device's properties and features, and it
    // is dropped before any other wgpu call on this device.
    let hal = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }?;
    let instance = hal.shared_instance();
    let physical = hal.raw_physical_device();
    if instance.instance_api_version() < vk::API_VERSION_1_1 {
        return Some((None, false));
    }
    let mut id = vk::PhysicalDeviceIDProperties::default();
    let api = {
        let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
        // SAFETY: the physical device belongs to this instance, which runs Vulkan 1.1 or later,
        // and the call only fills `properties` and the `id` chained to it.
        unsafe { instance.raw_instance().get_physical_device_properties2(physical, &mut properties) };
        properties.properties.api_version
    };
    let adapter = (id.device_luid_valid == vk::TRUE).then(|| AdapterId::from_luid_bytes(id.device_luid));
    // Reading a timeline semaphore's value from the CPU is a Vulkan 1.2 call.
    if api < vk::API_VERSION_1_2 || instance.instance_api_version() < vk::API_VERSION_1_2 {
        return Some((adapter, false));
    }
    let mut timeline = vk::PhysicalDeviceTimelineSemaphoreFeatures::default();
    {
        let mut features = vk::PhysicalDeviceFeatures2::default().push_next(&mut timeline);
        // SAFETY: as above; the call only fills `features` and the `timeline` chained to it.
        unsafe { instance.raw_instance().get_physical_device_features2(physical, &mut features) };
    }
    Some((adapter, timeline.timeline_semaphore == vk::TRUE))
}

/// A Direct3D 12 device on the adapter `adapter`, to open the ring's fences on.
fn d3d12_device(adapter: AdapterId) -> Result<ID3D12Device, Fallback> {
    let (low, high) = adapter.parts();
    // SAFETY: creating a DXGI factory has no precondition.
    let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1() }.map_err(|_| Fallback::ImportFailed)?;
    // SAFETY: `factory` is a valid factory; looking up an adapter has no other effect.
    let dxgi: IDXGIAdapter1 = unsafe { factory.EnumAdapterByLuid(LUID { LowPart: low, HighPart: high }) }.map_err(|_| Fallback::ImportFailed)?;
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: `dxgi` is a valid adapter and `device` receives the new device.
    unsafe { D3D12CreateDevice(&dxgi, D3D_FEATURE_LEVEL_11_0, &mut device) }.map_err(|_| Fallback::ImportFailed)?;
    device.ok_or(Fallback::ImportFailed)
}

/// Open `ring` on `device`, whose queue is `queue`. Returns [`Fallback::UnsupportedBackend`] when
/// the device does not run on Vulkan.
pub(crate) fn import(device: &wgpu::Device, queue: &wgpu::Queue, ring: &SharedRing) -> Result<Imported, Fallback> {
    let Some((compositor, timeline)) = adapter(device) else { return Err(Fallback::UnsupportedBackend) };
    let external_memory = device.features().contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32);
    check_import(Backend::Vulkan { external_memory, timeline }, ring.adapter(), compositor)?;
    if !ring.handles.complete() {
        return Err(Fallback::ImportFailed);
    }
    let d3d12 = d3d12_device(ring.adapter())?;
    let fence = |handle| -> Result<ID3D12Fence, Fallback> {
        let mut fence: Option<ID3D12Fence> = None;
        // SAFETY: `handle` is an open NT handle of a shared fence, kept open by `ring`, and
        // `fence` receives the opened fence.
        unsafe { d3d12.OpenSharedHandle(handle, &mut fence) }.map_err(|_| Fallback::ImportFailed)?;
        fence.ok_or(Fallback::ImportFailed)
    };
    let ready = fence(ring.handles.ready)?;
    let released = fence(ring.handles.released)?;
    // SAFETY: the fence is valid; reading its completed value does not wait.
    let signaled = unsafe { released.GetCompletedValue() };

    let size = wgpu::Extent3d { width: ring.width(), height: ring.height(), depth_or_array_layers: 1 };
    let desc = wgpu::TextureDescriptor {
        label: Some("shared capture"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Bgra8Unorm,
        usage: wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    };
    let hal_desc = wgpu::hal::TextureDescriptor {
        label: desc.label,
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: desc.dimension,
        format: desc.format,
        usage: wgpu::TextureUses::RESOURCE,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: Vec::new(),
    };
    let mut raw = Vec::with_capacity(ring.handles.textures.len());
    let (vk_device, family) = {
        // SAFETY: the guard is only used to create images, bind memory to them and clone the
        // device's function table, and it is dropped before the images are handed to wgpu.
        let Some(hal) = (unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }) else { return Err(Fallback::UnsupportedBackend) };
        for &handle in &ring.handles.textures {
            // SAFETY: the device has `VULKAN_EXTERNAL_MEMORY_WIN32`, `handle` is an open NT handle
            // of a single-sampled 2D BGRA8 texture of `size` with one mip level, as the capture
            // side created it, and `ring` keeps it open. The import does not take the handle.
            let texture = unsafe { hal.texture_from_d3d11_shared_handle(handle, &hal_desc) }.map_err(|_| Fallback::ImportFailed)?;
            raw.push(texture);
        }
        (hal.raw_device().clone(), hal.queue_family_index())
    };
    let textures: Vec<wgpu::Texture> = raw
        .into_iter()
        .map(|texture| {
            // SAFETY: `texture` was made on this device and matches `desc`. wgpu is told the image
            // is in `SHADER_READ_ONLY_OPTIMAL` (`RESOURCE`), the layout it has at every draw: each
            // draw's first barrier moves it there from `GENERAL`, the layout the capture side
            // writes it in, and wgpu records no layout change of its own for a texture that is
            // only sampled. wgpu does not clear a texture made from wgpu-hal.
            unsafe { device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(texture, &desc, wgpu::TextureUses::RESOURCE) }
        })
        .collect();
    let mut images = Vec::with_capacity(textures.len());
    for texture in &textures {
        // SAFETY: the guard is only used to read the image handle, which stays owned by wgpu.
        let image = unsafe { texture.as_hal::<wgpu::hal::api::Vulkan>().map(|t| t.raw_handle()) };
        images.push(image.ok_or(Fallback::ImportFailed)?);
    }

    let mut kind = vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE).initial_value(signaled);
    let info = vk::SemaphoreCreateInfo::default().push_next(&mut kind);
    // SAFETY: the device has timeline semaphores (checked above) and `info` describes one.
    let read = unsafe { vk_device.create_semaphore(&info, None) }.map_err(|_| Fallback::ImportFailed)?;
    Ok(Imported {
        textures,
        images,
        family,
        device: vk_device,
        queue: queue.clone(),
        read,
        ready,
        released,
        queued: AtomicU64::new(signaled),
        submitted: AtomicU64::new(signaled),
        signaled: AtomicU64::new(signaled),
    })
}

impl Imported {
    /// The ready fence's value now, without waiting. It also hands back the textures whose draws
    /// have finished.
    pub(crate) fn completed(&self) -> u64 {
        self.poll();
        // SAFETY: `ready` is a valid fence; reading its completed value does not wait.
        unsafe { self.ready.GetCompletedValue() }
    }

    /// The commands that pass texture `slot` from the capture side to this queue before the draw
    /// that reads it, and back after.
    pub(crate) fn acquire(&self, device: &wgpu::Device, slot: usize) -> Option<wgpu::CommandBuffer> {
        self.barrier(device, slot, true)
    }

    pub(crate) fn hand_back(&self, device: &wgpu::Device, slot: usize) -> Option<wgpu::CommandBuffer> {
        self.barrier(device, slot, false)
    }

    fn barrier(&self, wgpu_device: &wgpu::Device, slot: usize, acquire: bool) -> Option<wgpu::CommandBuffer> {
        let &image = self.images.get(slot)?;
        let (from, to) = if acquire { (vk::QUEUE_FAMILY_EXTERNAL, self.family) } else { (self.family, vk::QUEUE_FAMILY_EXTERNAL) };
        let (src_access, dst_access) =
            if acquire { (vk::AccessFlags::empty(), vk::AccessFlags::SHADER_READ) } else { (vk::AccessFlags::SHADER_READ, vk::AccessFlags::empty()) };
        let (src_stage, dst_stage) = if acquire {
            (vk::PipelineStageFlags::TOP_OF_PIPE, vk::PipelineStageFlags::FRAGMENT_SHADER)
        } else {
            (vk::PipelineStageFlags::FRAGMENT_SHADER, vk::PipelineStageFlags::BOTTOM_OF_PIPE)
        };
        let (old_layout, new_layout) = if acquire {
            (vk::ImageLayout::GENERAL, vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
        } else {
            (vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL, vk::ImageLayout::GENERAL)
        };
        let barrier = vk::ImageMemoryBarrier::default()
            .src_access_mask(src_access)
            .dst_access_mask(dst_access)
            .old_layout(old_layout)
            .new_layout(new_layout)
            .src_queue_family_index(from)
            .dst_queue_family_index(to)
            .image(image)
            .subresource_range(vk::ImageSubresourceRange {
                aspect_mask: vk::ImageAspectFlags::COLOR,
                base_mip_level: 0,
                level_count: 1,
                base_array_layer: 0,
                layer_count: 1,
            });
        // Between the two barriers the image is in `SHADER_READ_ONLY_OPTIMAL`, the layout wgpu
        // tracks for it, and outside them the capture side owns it in `GENERAL`, a layout that
        // keeps its contents.
        Some(barriers(wgpu_device, &self.device, src_stage, dst_stage, &[barrier]))
    }

    /// Hand the texture of the frame with value `value` back to the capture side once the work
    /// already submitted to the queue has finished, the draw that read it included. It is called
    /// after that submission: an empty submission signals `read` with `value`. A signal is applied
    /// to whichever submission comes next on the queue, from any thread, and every such submission
    /// comes after the draw, so its signal also covers the draw.
    pub(crate) fn release(&self, value: u64) {
        if value <= self.queued.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: the guard is only used to change the semaphores the next submission signals, and
        // it is dropped before any other wgpu call on this queue.
        if let Some(hal) = unsafe { self.queue.as_hal::<wgpu::hal::api::Vulkan>() } {
            if !hal.remove_signal_semaphore(self.read) {
                // The value queued before went out with a submission.
                self.submitted.store(self.queued.load(Ordering::Acquire), Ordering::Release);
            }
            hal.add_signal_semaphore(self.read, Some(value));
            self.queued.store(value, Ordering::Release);
        }
        self.queue.submit(std::iter::empty());
        self.poll();
    }

    /// Copy the value the queue has signaled on `read` to the released fence, without waiting.
    pub(crate) fn poll(&self) {
        // SAFETY: `read` is a timeline semaphore of this device, which runs Vulkan 1.2.
        let Ok(done) = (unsafe { self.device.get_semaphore_counter_value(self.read) }) else { return };
        if done > self.signaled.load(Ordering::Acquire) {
            // SAFETY: `released` is a valid fence; setting its value from the CPU does not wait. A
            // failure leaves the texture to be taken back after `RECLAIM`.
            if unsafe { self.released.Signal(done) }.is_ok() {
                self.signaled.store(done, Ordering::Release);
            }
        }
    }
}

/// A command buffer that holds only `images`' barriers. wgpu does not let an encoder that recorded
/// raw commands record its own, so the barriers go in a command buffer of their own, submitted
/// next to wgpu's.
pub(crate) fn barriers(
    wgpu_device: &wgpu::Device,
    device: &ash::Device,
    src_stage: vk::PipelineStageFlags,
    dst_stage: vk::PipelineStageFlags,
    images: &[vk::ImageMemoryBarrier<'_>],
) -> wgpu::CommandBuffer {
    let mut encoder = wgpu_device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("shared texture barriers") });
    // SAFETY: the encoder belongs to `wgpu_device`, whose Vulkan device is `device`, and the
    // images are images of that device. The encoder records nothing but these barriers.
    unsafe {
        encoder.as_hal_mut::<wgpu::hal::api::Vulkan, _, _>(|hal| {
            if let Some(hal) = hal {
                device.cmd_pipeline_barrier(hal.raw_handle(), src_stage, dst_stage, vk::DependencyFlags::empty(), &[], &[], images);
            }
        })
    };
    encoder.finish()
}

impl Drop for Imported {
    fn drop(&mut self) {
        // SAFETY: as in `release`; a signal not submitted yet is taken off the list.
        let unsubmitted = unsafe { self.queue.as_hal::<wgpu::hal::api::Vulkan>() }.is_some_and(|hal| hal.remove_signal_semaphore(self.read));
        // The newest value a submission still signals.
        let target = if unsubmitted { self.submitted.load(Ordering::Acquire) } else { self.queued.load(Ordering::Acquire) };
        self.poll();
        // SAFETY: `read` is a timeline semaphore of this device, which runs Vulkan 1.2.
        let done = unsafe { self.device.get_semaphore_counter_value(self.read) }.is_ok_and(|done| done >= target);
        if done {
            // SAFETY: no submission still signals `read`: the one not submitted was taken off the
            // list, and the submitted ones have finished.
            unsafe { self.device.destroy_semaphore(self.read, None) };
            return;
        }
        // A draw still runs: once the queue has finished the work submitted so far, the released
        // fence gets the last value and the semaphore is destroyed, without waiting here.
        let (device, read, released) = (self.device.clone(), self.read, self.released.clone());
        self.queue.on_submitted_work_done(move || {
            // SAFETY: `released` is a valid fence; reading and setting its value from the CPU does
            // not wait. It is only raised, so a value another import set later is kept.
            unsafe {
                if released.GetCompletedValue() < target {
                    let _ = released.Signal(target);
                }
            }
            // SAFETY: every submission that signals `read` was made before this callback was
            // registered, so all of them have finished, and nothing uses `read` afterward.
            unsafe { device.destroy_semaphore(read, None) };
        });
    }
}
