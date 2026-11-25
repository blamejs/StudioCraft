//! The compositor side on Windows when the compositor's wgpu device runs on Vulkan: a ring's shared
//! textures opened as Vulkan images through `VK_KHR_external_memory_win32`, and its two fences
//! opened on a Direct3D 12 device of the same adapter; and an export ring's NV12 textures opened
//! the same way and written by the compositor's queue. Every wgpu-hal, Vulkan and Direct3D 12 call
//! of the import and the export is here.
//!
//! wgpu opens its Vulkan devices without `VK_KHR_external_semaphore_win32`, and the fences are not
//! imported as Vulkan semaphores. The importer reads the ready fence from the CPU. A submission after
//! a draw signals a timeline semaphore of the importer's own with the frame's value, and the
//! importer copies the semaphore's value to the released fence from the CPU on a later call. Each
//! draw takes the texture's ownership from the external queue family before it reads the texture,
//! and hands it back after. The export works the same way the other way around: a submission after
//! a copy signals a timeline semaphore, and its value is copied to the export ring's ready fence.

use std::sync::atomic::{AtomicU64, Ordering};

use ash::vk;
use windows::Win32::Foundation::{HANDLE, LUID};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
use windows::Win32::Graphics::Direct3D12::{D3D12CreateDevice, ID3D12Device, ID3D12Fence};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory4};

use crate::export::ExportRing;
use crate::fallback::{Backend, Fallback, check_import, open_all};
use crate::ring::{AdapterId, SharedRing};

/// A ring opened on one Vulkan device: its textures as wgpu textures, and its two fences.
pub(crate) struct Imported {
    pub(crate) textures: Vec<wgpu::Texture>,
    images: Vec<vk::Image>,
    family: u32,
    device: ash::Device,
    ready: ID3D12Fence,
    /// Signaled by the compositor's queue with a frame's value once the draw that read it is done,
    /// and copied to the ring's released fence.
    released: Timeline,
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

/// The shared fence of NT handle `handle`, opened on `d3d12`.
fn open_fence(d3d12: &ID3D12Device, handle: HANDLE) -> Result<ID3D12Fence, Fallback> {
    let mut fence: Option<ID3D12Fence> = None;
    // SAFETY: `handle` is an open NT handle of a shared fence, kept open by its ring, and `fence`
    // receives the opened fence.
    unsafe { d3d12.OpenSharedHandle(handle, &mut fence) }.map_err(|_| Fallback::ImportFailed)?;
    fence.ok_or(Fallback::ImportFailed)
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
    let ready = open_fence(&d3d12, ring.handles.ready)?;
    let released = open_fence(&d3d12, ring.handles.released)?;

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
    let (raw, vk_device, family) = {
        // SAFETY: the guard is only used to create images, bind memory to them, destroy them after
        // a failed import and clone the device's function table, and it is dropped before the
        // images are handed to wgpu.
        let Some(hal) = (unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }) else { return Err(Fallback::UnsupportedBackend) };
        let raw = open_all(
            &ring.handles.textures,
            |handle| {
                // SAFETY: the device has `VULKAN_EXTERNAL_MEMORY_WIN32` and runs Vulkan 1.1 or later
                // (`check_import` passed), `handle` is an open NT handle of a single-sampled 2D
                // BGRA8 texture of `size` with one mip level, as the capture side created it, and
                // `ring` keeps it open; `hal_desc` describes it, only sampled. The import does not
                // take the handle, and a failed import leaves nothing behind.
                unsafe { import_d3d11(&hal, handle, &hal_desc, BGRA) }
            },
            |texture| {
                // SAFETY: `texture` was made on this device, nothing else holds it, and no
                // submission uses it. A wgpu-hal texture is not freed when it is dropped.
                unsafe { wgpu::hal::Device::destroy_texture(&*hal, texture) }
            },
        )?;
        (raw, hal.raw_device().clone(), hal.queue_family_index())
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
    let released = Timeline::new(vk_device.clone(), queue.clone(), released)?;
    Ok(Imported { textures, images, family, device: vk_device, ready, released })
}

impl Imported {
    /// The ready fence's value now, without waiting; [`REMOVED`](crate::export::REMOVED) once the
    /// device that holds the fence has been removed.
    pub(crate) fn completed(&self) -> u64 {
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
            .subresource_range(range(vk::ImageAspectFlags::COLOR));
        let device = &self.device;
        // Between the two barriers the image is in `SHADER_READ_ONLY_OPTIMAL`, the layout wgpu
        // tracks for it, and outside them the capture side owns it in `GENERAL`, a layout that
        // keeps its contents.
        Some(raw_commands(wgpu_device, "shared capture ownership", |cmd| {
            // SAFETY: `cmd` is a command buffer of `device` being recorded, and the image is an
            // image of that device.
            unsafe { device.cmd_pipeline_barrier(cmd, src_stage, dst_stage, vk::DependencyFlags::empty(), &[], &[], &[barrier]) };
        }))
    }

    /// Hand the texture of the frame with value `value` back to the capture side once the work
    /// already submitted to the queue has finished, the draw that read it included.
    pub(crate) fn release(&self, value: u64) {
        self.released.after_submitted(value);
    }

    /// Copy the value the queue has reached to the released fence, without waiting. Returns false
    /// when the semaphore's value could not be read or the released fence could not be set: the
    /// device was lost, removed or out of memory, so the textures would not go back.
    pub(crate) fn poll(&self) -> bool {
        self.released.poll()
    }
}

/// The whole of one mip level and layer of an image, for the aspects `aspect`.
fn range(aspect: vk::ImageAspectFlags) -> vk::ImageSubresourceRange {
    vk::ImageSubresourceRange { aspect_mask: aspect, base_mip_level: 0, level_count: 1, base_array_layer: 0, layer_count: 1 }
}

/// The barriers around the copies into the NV12 image `image`: the first takes it from the
/// external queue family to `family`, its old contents discarded, and the second hands it back in
/// `GENERAL`. The image is not disjoint, so the barriers name the color aspect, which covers both
/// planes (VUID-VkImageMemoryBarrier-image-09242); only the copies name a plane.
pub(crate) fn export_barriers(image: vk::Image, family: u32) -> [vk::ImageMemoryBarrier<'static>; 2] {
    let acquire = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::empty())
        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .old_layout(vk::ImageLayout::UNDEFINED)
        .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .src_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
        .dst_queue_family_index(family)
        .image(image)
        .subresource_range(range(vk::ImageAspectFlags::COLOR));
    let hand_back = vk::ImageMemoryBarrier::default()
        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
        .dst_access_mask(vk::AccessFlags::empty())
        .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
        .new_layout(vk::ImageLayout::GENERAL)
        .src_queue_family_index(family)
        .dst_queue_family_index(vk::QUEUE_FAMILY_EXTERNAL)
        .image(image)
        .subresource_range(range(vk::ImageAspectFlags::COLOR));
    [acquire, hand_back]
}

/// A command buffer that holds only the raw commands `record` records into the Vulkan command
/// buffer it is given. wgpu does not let an encoder that recorded raw commands record its own, so
/// raw commands go in a command buffer of their own, submitted next to wgpu's.
fn raw_commands(wgpu_device: &wgpu::Device, label: &str, record: impl FnOnce(vk::CommandBuffer)) -> wgpu::CommandBuffer {
    let mut encoder = wgpu_device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some(label) });
    // SAFETY: the encoder belongs to `wgpu_device` and records nothing but what `record` records;
    // the command buffer handle is only used while the closure runs.
    unsafe {
        encoder.as_hal_mut::<wgpu::hal::api::Vulkan, _, _>(|hal| {
            if let Some(hal) = hal {
                record(hal.raw_handle());
            }
        })
    };
    encoder.finish()
}

/// A timeline semaphore that the compositor's queue signals after the work submitted so far, and a
/// shared Direct3D 12 fence set from the CPU to the value the semaphore has reached. Nothing waits:
/// the value is copied on the next [`Timeline::poll`].
struct Timeline {
    device: ash::Device,
    queue: wgpu::Queue,
    semaphore: vk::Semaphore,
    fence: ID3D12Fence,
    /// The newest value queued on `semaphore`.
    queued: AtomicU64,
    /// The newest value on `semaphore` that went out with a submission, apart from `queued`.
    submitted: AtomicU64,
    /// The newest value set on `fence`.
    signaled: AtomicU64,
}

impl Timeline {
    /// A semaphore of `device` that starts at `fence`'s value. The device must have timeline
    /// semaphores.
    fn new(device: ash::Device, queue: wgpu::Queue, fence: ID3D12Fence) -> Result<Timeline, Fallback> {
        // SAFETY: the fence is valid; reading its completed value does not wait.
        let start = unsafe { fence.GetCompletedValue() };
        let mut kind = vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE).initial_value(start);
        let info = vk::SemaphoreCreateInfo::default().push_next(&mut kind);
        // SAFETY: the device has timeline semaphores (its caller checked) and `info` describes one.
        let semaphore = unsafe { device.create_semaphore(&info, None) }.map_err(|_| Fallback::ImportFailed)?;
        Ok(Timeline {
            device,
            queue,
            semaphore,
            fence,
            queued: AtomicU64::new(start),
            submitted: AtomicU64::new(start),
            signaled: AtomicU64::new(start),
        })
    }

    /// Set the fence to `value` once the work already submitted to the queue has finished. It is
    /// called after that submission: an empty submission signals the semaphore with `value`. A
    /// signal is applied to whichever submission comes next on the queue, from any thread, and
    /// every such submission comes after the work, so its signal also covers the work.
    fn after_submitted(&self, value: u64) {
        if value <= self.queued.load(Ordering::Acquire) {
            return;
        }
        // SAFETY: the guard is only used to change the semaphores the next submission signals, and
        // it is dropped before any other wgpu call on this queue.
        if let Some(hal) = unsafe { self.queue.as_hal::<wgpu::hal::api::Vulkan>() } {
            if !hal.remove_signal_semaphore(self.semaphore) {
                // The value queued before went out with a submission.
                self.submitted.store(self.queued.load(Ordering::Acquire), Ordering::Release);
            }
            hal.add_signal_semaphore(self.semaphore, Some(value));
            self.queued.store(value, Ordering::Release);
        }
        self.queue.submit(std::iter::empty());
        self.poll();
    }

    /// Copy the value the queue has signaled on the semaphore to the fence, without waiting.
    /// Returns false when the semaphore's value could not be read or the fence could not be set.
    /// Both calls fail only for a lost or removed device or for lack of memory.
    fn poll(&self) -> bool {
        // SAFETY: `semaphore` is a timeline semaphore of this device, which runs Vulkan 1.2.
        let Ok(done) = (unsafe { self.device.get_semaphore_counter_value(self.semaphore) }) else { return false };
        if done > self.signaled.load(Ordering::Acquire) {
            // SAFETY: `fence` is a valid fence; setting its value from the CPU does not wait.
            if unsafe { self.fence.Signal(done) }.is_err() {
                return false;
            }
            self.signaled.store(done, Ordering::Release);
        }
        true
    }
}

impl Drop for Timeline {
    fn drop(&mut self) {
        // SAFETY: as in `after_submitted`; a signal not submitted yet is taken off the list.
        let unsubmitted = unsafe { self.queue.as_hal::<wgpu::hal::api::Vulkan>() }.is_some_and(|hal| hal.remove_signal_semaphore(self.semaphore));
        // The newest value a submission still signals.
        let target = if unsubmitted { self.submitted.load(Ordering::Acquire) } else { self.queued.load(Ordering::Acquire) };
        self.poll();
        // SAFETY: `semaphore` is a timeline semaphore of this device, which runs Vulkan 1.2.
        let reached = unsafe { self.device.get_semaphore_counter_value(self.semaphore) }.ok().filter(|&done| done >= target);
        if let Some(done) = reached {
            // The value read here, not the one `poll` read before it, says the work has finished:
            // the fence gets it before the semaphore goes.
            // SAFETY: `fence` is a valid fence; reading and setting its value from the CPU does
            // not wait. It is only raised, so a value set later by another side is kept.
            unsafe {
                if self.fence.GetCompletedValue() < done {
                    let _ = self.fence.Signal(done);
                }
            }
            // SAFETY: no submission still signals the semaphore: the one not submitted was taken
            // off the list, and the submitted ones have finished.
            unsafe { self.device.destroy_semaphore(self.semaphore, None) };
            return;
        }
        // Work still runs: once the queue has finished the work submitted so far, the fence gets
        // the last value and the semaphore is destroyed, without waiting here.
        let (device, semaphore, fence) = (self.device.clone(), self.semaphore, self.fence.clone());
        self.queue.on_submitted_work_done(move || {
            // SAFETY: `fence` is a valid fence; reading and setting its value from the CPU does
            // not wait. It is only raised, so a value set later by another side is kept.
            unsafe {
                if fence.GetCompletedValue() < target {
                    let _ = fence.Signal(target);
                }
            }
            // SAFETY: every submission that signals the semaphore was made before this callback was
            // registered, so all of them have finished, and nothing uses it afterward.
            unsafe { device.destroy_semaphore(semaphore, None) };
        });
    }
}

/// An export ring's NV12 textures opened as Vulkan images on the compositor's device, and its ready
/// fence. The images stay wgpu-hal textures: wgpu cannot write NV12 textures, and only the raw
/// copies of [`Exported::copy`] touch them.
pub(crate) struct Exported {
    /// `None` only while being dropped.
    textures: Option<Vec<wgpu::hal::vulkan::Texture>>,
    images: Vec<vk::Image>,
    family: u32,
    device: ash::Device,
    wgpu_device: wgpu::Device,
    queue: wgpu::Queue,
    /// Signaled by the compositor's queue with a frame's value once its copy is done, and copied
    /// to the ring's ready fence.
    ready: Timeline,
}

/// Open `ring`'s textures and ready fence on `device`. Returns [`Fallback::UnsupportedBackend`]
/// when the device does not run on Vulkan.
pub(crate) fn export(device: &wgpu::Device, queue: &wgpu::Queue, ring: &ExportRing) -> Result<Exported, Fallback> {
    let Some((compositor, timeline)) = adapter(device) else { return Err(Fallback::UnsupportedBackend) };
    let external_memory = device.features().contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32);
    check_import(Backend::Vulkan { external_memory, timeline }, ring.adapter(), compositor)?;
    let Some(nv12) = &ring.nv12 else { return Err(Fallback::ImportFailed) };
    if !nv12.handles.complete() {
        return Err(Fallback::ImportFailed);
    }
    let d3d12 = d3d12_device(ring.adapter())?;
    let ready = open_fence(&d3d12, nv12.handles.ready)?;
    let size = wgpu::Extent3d { width: ring.width(), height: ring.height(), depth_or_array_layers: 1 };
    let hal_desc = wgpu::hal::TextureDescriptor {
        label: Some("program to encoder"),
        size,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::NV12,
        usage: wgpu::TextureUses::COPY_DST,
        memory_flags: wgpu::hal::MemoryFlags::empty(),
        view_formats: Vec::new(),
    };
    let mut textures = Vec::with_capacity(nv12.handles.textures.len());
    let (vk_device, family) = {
        // SAFETY: the guard is only used to create images, bind memory to them and clone the
        // device's function table, and it is dropped before the function returns.
        let Some(hal) = (unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }) else { return Err(Fallback::UnsupportedBackend) };
        for &handle in &nv12.handles.textures {
            // SAFETY: the device has `VULKAN_EXTERNAL_MEMORY_WIN32` and runs Vulkan 1.1 or later
            // (`check_import` passed), `handle` is an open NT handle of a single-sampled 2D NV12
            // texture of `size` with one mip level, as the encoder side created it, and `ring`
            // keeps it open; `hal_desc` describes it as a copy destination. The import does not
            // take the handle.
            match unsafe { import_d3d11(&hal, handle, &hal_desc, NV12) } {
                Ok(texture) => textures.push(texture),
                Err(_) => {
                    use wgpu::hal::Device as _;
                    for texture in textures {
                        // SAFETY: no command has used these textures yet.
                        unsafe { hal.destroy_texture(texture) };
                    }
                    return Err(Fallback::ImportFailed);
                }
            }
        }
        (hal.raw_device().clone(), hal.queue_family_index())
    };
    // SAFETY: the image handles stay owned by the wgpu-hal textures, which outlive their use.
    let images = textures.iter().map(|t| unsafe { t.raw_handle() }).collect();
    let ready = match Timeline::new(vk_device.clone(), queue.clone(), ready) {
        Ok(ready) => ready,
        Err(reason) => {
            // SAFETY: the guard is only used to destroy the textures, which no command has used
            // and nothing else refers to.
            if let Some(hal) = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() } {
                use wgpu::hal::Device as _;
                for texture in textures {
                    // SAFETY: as above.
                    unsafe { hal.destroy_texture(texture) };
                }
            }
            return Err(reason);
        }
    };
    Ok(Exported { textures: Some(textures), images, family, device: vk_device, wgpu_device: device.clone(), queue: queue.clone(), ready })
}

/// The format, flags and usage an image opened from a shared Direct3D 11 texture is made with:
/// the ones wgpu-hal gives a texture of the same wgpu format and usage.
#[derive(Clone, Copy)]
struct Shape {
    format: vk::Format,
    flags: vk::ImageCreateFlags,
    usage: vk::ImageUsageFlags,
}

/// A capture ring's BGRA8 texture, only sampled.
const BGRA: Shape = Shape { format: vk::Format::B8G8R8A8_UNORM, flags: vk::ImageCreateFlags::empty(), usage: vk::ImageUsageFlags::SAMPLED };

/// An export ring's NV12 texture, only a copy destination: wgpu-hal gives a multi-planar format
/// these flags.
const NV12: Shape = Shape {
    format: vk::Format::G8_B8R8_2PLANE_420_UNORM,
    flags: vk::ImageCreateFlags::from_raw(vk::ImageCreateFlags::MUTABLE_FORMAT.as_raw() | vk::ImageCreateFlags::EXTENDED_USAGE.as_raw()),
    usage: vk::ImageUsageFlags::TRANSFER_DST,
};

/// Open the shared texture `handle` as an image of `hal` made as `shape`, with dedicated memory
/// bound to it, as wgpu-hal's `texture_from_d3d11_shared_handle` does, except that: the driver
/// must report that it imports a Direct3D 11 texture as such an image; the memory type is one the
/// handle allows as well as the image; and a failed allocation or bind destroys the image and
/// frees the memory it made.
///
/// # Safety
///
/// `hal` has `VULKAN_EXTERNAL_MEMORY_WIN32` and runs Vulkan 1.1 or later, and `handle` is an open
/// NT handle of a single-sampled 2D texture of `desc`'s size with one mip level, in the format of
/// `shape`, which stays open for the call. `desc` describes that texture with the usage of
/// `shape`.
unsafe fn import_d3d11(
    hal: &wgpu::hal::vulkan::Device,
    handle: HANDLE,
    desc: &wgpu::hal::TextureDescriptor,
    shape: Shape,
) -> Result<wgpu::hal::vulkan::Texture, Fallback> {
    let device = hal.raw_device();
    let instance = hal.shared_instance().raw_instance();
    let kind = vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE;
    let mut external_info = vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(kind);
    let format_info = vk::PhysicalDeviceImageFormatInfo2::default()
        .format(shape.format)
        .ty(vk::ImageType::TYPE_2D)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(shape.usage)
        .flags(shape.flags)
        .push_next(&mut external_info);
    let mut external_properties = vk::ExternalImageFormatProperties::default();
    {
        let mut properties = vk::ImageFormatProperties2::default().push_next(&mut external_properties);
        // SAFETY: the physical device belongs to this instance, which runs Vulkan 1.1 or later;
        // the call only reads `format_info` and fills `properties` and the structure chained to
        // it, and both chains live for the call.
        unsafe { instance.get_physical_device_image_format_properties2(hal.raw_physical_device(), &format_info, &mut properties) }
            .map_err(|_| Fallback::ImportFailed)?;
    }
    let memory = external_properties.external_memory_properties;
    if !memory.external_memory_features.contains(vk::ExternalMemoryFeatureFlags::IMPORTABLE) || !memory.compatible_handle_types.contains(kind) {
        return Err(Fallback::ImportFailed);
    }
    let mut handle_properties = vk::MemoryWin32HandlePropertiesKHR::default();
    // SAFETY: the device was opened with `VK_KHR_external_memory_win32`, whose functions this
    // loads; `handle` is an open NT handle of a Direct3D 11 texture, and the call only fills
    // `handle_properties`.
    unsafe {
        ash::khr::external_memory_win32::Device::new(instance, device).get_memory_win32_handle_properties(kind, handle.0 as _, &mut handle_properties)
    }
    .map_err(|_| Fallback::ImportFailed)?;
    let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(kind);
    let info = vk::ImageCreateInfo::default()
        .flags(shape.flags)
        .image_type(vk::ImageType::TYPE_2D)
        .format(shape.format)
        .extent(vk::Extent3D { width: desc.size.width, height: desc.size.height, depth: 1 })
        .mip_levels(1)
        .array_layers(1)
        .samples(vk::SampleCountFlags::TYPE_1)
        .tiling(vk::ImageTiling::OPTIMAL)
        .usage(shape.usage)
        .sharing_mode(vk::SharingMode::EXCLUSIVE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .push_next(&mut external);
    // SAFETY: `info` describes a 2D image for an external Direct3D 11 texture with the format,
    // flags and usage the driver reported it imports, and its chain lives for the call.
    let image = unsafe { device.create_image(&info, None) }.map_err(|_| Fallback::ImportFailed)?;
    // SAFETY: `image` is an image of `device` without memory.
    let requirements = unsafe { device.get_image_memory_requirements(image) };
    // SAFETY: the physical device is the one `device` was created from.
    let properties = unsafe { instance.get_physical_device_memory_properties(hal.raw_physical_device()) };
    let local = properties.memory_types_as_slice().iter().map(|ty| ty.property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL));
    let index = crate::fallback::memory_type(requirements.memory_type_bits, handle_properties.memory_type_bits, local);
    let destroy_image = || {
        // SAFETY: `image` is an image of `device` that nothing has used or bound memory to.
        unsafe { device.destroy_image(image, None) };
    };
    let Some(index) = index else {
        destroy_image();
        return Err(Fallback::ImportFailed);
    };
    let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
    let mut import =
        vk::ImportMemoryWin32HandleInfoKHR::default().handle_type(vk::ExternalMemoryHandleTypeFlags::D3D11_TEXTURE).handle(handle.0 as _);
    let allocate = vk::MemoryAllocateInfo::default()
        .allocation_size(requirements.size)
        .memory_type_index(index)
        .push_next(&mut dedicated)
        .push_next(&mut import);
    // SAFETY: `allocate` imports the open handle as dedicated memory of `image`, and its chain lives
    // for the call. The import does not take the handle.
    let memory = match unsafe { device.allocate_memory(&allocate, None) } {
        Ok(memory) => memory,
        Err(_) => {
            destroy_image();
            return Err(Fallback::ImportFailed);
        }
    };
    // SAFETY: `memory` was allocated for `image` with its requirements, and nothing is bound yet.
    if unsafe { device.bind_image_memory(image, memory, 0) }.is_err() {
        // SAFETY: nothing uses `memory`, which is bound to no image.
        unsafe { device.free_memory(memory, None) };
        destroy_image();
        return Err(Fallback::ImportFailed);
    }
    // SAFETY: `image` is an image of `hal` described by `desc` with `memory` bound to it; wgpu-hal
    // takes both and destroys them with the texture.
    Ok(unsafe { hal.texture_from_raw(image, desc, None, wgpu::hal::vulkan::TextureMemory::Dedicated(memory)) })
}

impl Exported {
    /// The commands that copy `planes`, the luma (R8) and chroma (RG8) textures of one frame, into
    /// texture `slot`'s two planes. The planes must be in `TRANSFER_SRC_OPTIMAL`: the caller's last
    /// use of them, already recorded, is a copy from them. The NV12 image is taken from the
    /// external queue family, its old contents discarded, and handed back in `GENERAL`.
    pub(crate) fn copy(&self, planes: &[wgpu::Texture], slot: usize) -> Result<wgpu::CommandBuffer, Fallback> {
        let &dst = self.images.get(slot).ok_or(Fallback::ImportFailed)?;
        let (Some(luma), Some(chroma)) = (planes.first(), planes.get(1)) else { return Err(Fallback::ImportFailed) };
        // SAFETY: the guards are only used to read the image handles, which stay owned by wgpu.
        let raw = |t: &wgpu::Texture| unsafe { t.as_hal::<wgpu::hal::api::Vulkan>().map(|h| h.raw_handle()) };
        let (Some(luma_image), Some(chroma_image)) = (raw(luma), raw(chroma)) else { return Err(Fallback::UnsupportedBackend) };
        let [acquire, hand_back] = export_barriers(dst, self.family);
        let layers = |aspect| vk::ImageSubresourceLayers { aspect_mask: aspect, mip_level: 0, base_array_layer: 0, layer_count: 1 };
        let region = |aspect, texture: &wgpu::Texture| vk::ImageCopy {
            src_subresource: layers(vk::ImageAspectFlags::COLOR),
            src_offset: vk::Offset3D::default(),
            dst_subresource: layers(aspect),
            dst_offset: vk::Offset3D::default(),
            extent: vk::Extent3D { width: texture.width(), height: texture.height(), depth: 1 },
        };
        let (luma_region, chroma_region) = (region(vk::ImageAspectFlags::PLANE_0, luma), region(vk::ImageAspectFlags::PLANE_1, chroma));
        let device = &self.device;
        Ok(raw_commands(&self.wgpu_device, "program to encoder", |cmd| {
            // SAFETY: `cmd` is a command buffer of `device` being recorded. The plane images are
            // images of the device in `TRANSFER_SRC_OPTIMAL`, `dst` is an NV12 image of the device,
            // and each region lies inside both images: `Exporter::copy` refuses planes unless the
            // luma plane is the NV12 image's size and the chroma plane half of it. The barriers
            // move `dst` from the encoder side to this queue and back around the copies.
            unsafe {
                device.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[acquire],
                );
                device.cmd_copy_image(
                    cmd,
                    luma_image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    dst,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[luma_region],
                );
                device.cmd_copy_image(
                    cmd,
                    chroma_image,
                    vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                    dst,
                    vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                    &[chroma_region],
                );
                device.cmd_pipeline_barrier(
                    cmd,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[],
                    &[hand_back],
                );
            }
        }))
    }

    /// Set the ready fence to `value` once the work already submitted to the queue, the copy
    /// included, has finished.
    pub(crate) fn signal(&self, value: u64) {
        self.ready.after_submitted(value);
    }

    /// Copy the value the queue has reached to the ready fence, without waiting. Returns false
    /// when the ready fence could not be set.
    pub(crate) fn poll(&self) -> bool {
        self.ready.poll()
    }
}

impl Drop for Exported {
    fn drop(&mut self) {
        // A copy into the images may still run: they are destroyed once the queue has finished the
        // work submitted so far, without waiting here.
        let Some(textures) = self.textures.take() else { return };
        let device = self.wgpu_device.clone();
        self.queue.on_submitted_work_done(move || {
            use wgpu::hal::Device as _;
            // SAFETY: the guard is only used to destroy the images, which no submitted work uses
            // any more, and nothing else refers to them.
            if let Some(hal) = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() } {
                for texture in textures {
                    // SAFETY: as above.
                    unsafe { hal.destroy_texture(texture) };
                }
            }
        });
    }
}
