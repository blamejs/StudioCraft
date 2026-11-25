//! The compositor side on Windows: a ring's shared textures and fences opened on a wgpu Direct3D
//! 12 device, and an export ring's NV12 textures written by that device. Every wgpu-hal and
//! Direct3D 12 call of the import and the export is here.

use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device, ID3D12Fence, ID3D12Resource};

use crate::export::ExportRing;
use crate::fallback::{Backend, Fallback, check_import};
use crate::ring::{AdapterId, SharedRing};

/// A ring opened on one device: its textures as wgpu textures, and its two fences.
pub(crate) struct Imported {
    pub(crate) textures: Vec<wgpu::Texture>,
    ready: ID3D12Fence,
    released: ID3D12Fence,
    queue: ID3D12CommandQueue,
}

/// The device's Direct3D 12 device and queue, or `None` on another backend.
fn raw(device: &wgpu::Device) -> Option<(ID3D12Device, ID3D12CommandQueue)> {
    // SAFETY: the guard is only used to clone the two COM pointers, which keeps them alive on
    // their own, and it is dropped before any other wgpu call on this device.
    let hal = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }?;
    Some((hal.raw_device().clone(), hal.raw_queue().clone()))
}

/// Open `ring` on `device`.
pub(crate) fn import(device: &wgpu::Device, ring: &SharedRing) -> Result<Imported, Fallback> {
    let Some((raw_device, queue)) = raw(device) else { return Err(Fallback::UnsupportedBackend) };
    // SAFETY: `raw_device` is a valid device; reading its adapter's identifier has no other effect.
    let luid = unsafe { raw_device.GetAdapterLuid() };
    check_import(Backend::Dx12, ring.adapter(), Some(AdapterId::from_parts(luid.LowPart, luid.HighPart)))?;
    if !ring.handles.complete() {
        return Err(Fallback::ImportFailed);
    }
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
    let mut textures = Vec::with_capacity(ring.handles.textures.len());
    for &handle in &ring.handles.textures {
        let mut resource: Option<ID3D12Resource> = None;
        // SAFETY: `handle` is an open NT handle of a shared texture, kept open by `ring`, and
        // `resource` receives the opened resource.
        unsafe { raw_device.OpenSharedHandle(handle, &mut resource) }.map_err(|_| Fallback::ImportFailed)?;
        let resource = resource.ok_or(Fallback::ImportFailed)?;
        // SAFETY: the resource is a single-sampled 2D BGRA8 texture of `size` with one mip level,
        // as the capture side created it, and it is open on this device.
        let hal = unsafe { wgpu::hal::dx12::Device::texture_from_raw(resource, desc.format, desc.dimension, size, 1, 1) };
        // SAFETY: `hal` was made on this device's Direct3D 12 device and matches `desc`. It starts
        // in the common state, which `UNINITIALIZED` maps to, and wgpu does not clear it.
        let texture = unsafe { device.create_texture_from_hal::<wgpu::hal::api::Dx12>(hal, &desc, wgpu::TextureUses::UNINITIALIZED) };
        textures.push(texture);
    }
    let fence = |handle| -> Result<ID3D12Fence, Fallback> {
        let mut fence: Option<ID3D12Fence> = None;
        // SAFETY: `handle` is an open NT handle of a shared fence, kept open by `ring`.
        unsafe { raw_device.OpenSharedHandle(handle, &mut fence) }.map_err(|_| Fallback::ImportFailed)?;
        fence.ok_or(Fallback::ImportFailed)
    };
    let ready = fence(ring.handles.ready)?;
    let released = fence(ring.handles.released)?;
    Ok(Imported { textures, ready, released, queue })
}

impl Imported {
    /// The ready fence's value now, without waiting; [`REMOVED`](crate::export::REMOVED) once the
    /// device that holds the fence has been removed.
    pub(crate) fn completed(&self) -> u64 {
        // SAFETY: `ready` is a valid fence; reading its completed value does not wait.
        unsafe { self.ready.GetCompletedValue() }
    }

    /// Signal `value` on the released fence after the work already submitted to the queue.
    pub(crate) fn release(&self, value: u64) -> Result<(), Fallback> {
        // SAFETY: `queue` is the device's queue, which is free-threaded, and `released` is a fence
        // open on the same device.
        unsafe { self.queue.Signal(&self.released, value) }.map_err(|_| Fallback::ImportFailed)
    }
}

/// The adapter of `device`, or `None` when it does not run on Direct3D 12.
pub(crate) fn adapter(device: &wgpu::Device) -> Option<AdapterId> {
    let (raw_device, _) = raw(device)?;
    // SAFETY: `raw_device` is a valid device; reading its adapter's identifier has no other effect.
    let luid = unsafe { raw_device.GetAdapterLuid() };
    Some(AdapterId::from_parts(luid.LowPart, luid.HighPart))
}

/// An export ring's NV12 textures and its ready fence opened on a wgpu Direct3D 12 device. The
/// textures stay wgpu-hal textures: wgpu cannot write NV12 textures, and only the raw copies of
/// [`Exported::copy`] touch them.
pub(crate) struct Exported {
    /// `None` only while being dropped.
    textures: Option<Vec<wgpu::hal::dx12::Texture>>,
    ready: ID3D12Fence,
    queue: ID3D12CommandQueue,
    wgpu_queue: wgpu::Queue,
}

/// Open `ring`'s textures and ready fence on `device`. Returns [`Fallback::UnsupportedBackend`]
/// when the device does not run on Direct3D 12.
pub(crate) fn export(device: &wgpu::Device, queue: &wgpu::Queue, ring: &ExportRing) -> Result<Exported, Fallback> {
    let Some((raw_device, raw_queue)) = raw(device) else { return Err(Fallback::UnsupportedBackend) };
    let Some(nv12) = &ring.nv12 else { return Err(Fallback::ImportFailed) };
    if !nv12.handles.complete() {
        return Err(Fallback::ImportFailed);
    }
    let size = wgpu::Extent3d { width: ring.width(), height: ring.height(), depth_or_array_layers: 1 };
    let mut textures = Vec::with_capacity(nv12.handles.textures.len());
    for &handle in &nv12.handles.textures {
        let mut resource: Option<ID3D12Resource> = None;
        // SAFETY: `handle` is an open NT handle of a shared texture, kept open by `ring`, and
        // `resource` receives the opened resource.
        unsafe { raw_device.OpenSharedHandle(handle, &mut resource) }.map_err(|_| Fallback::ImportFailed)?;
        let resource = resource.ok_or(Fallback::ImportFailed)?;
        let (nv12, d2) = (wgpu::TextureFormat::NV12, wgpu::TextureDimension::D2);
        // SAFETY: the resource is a single-sampled 2D NV12 texture of `size` with one mip level, as
        // the encoder side created it, and it is open on this device.
        let texture = unsafe { wgpu::hal::dx12::Device::texture_from_raw(resource, nv12, d2, size, 1, 1) };
        textures.push(texture);
    }
    let mut ready: Option<ID3D12Fence> = None;
    // SAFETY: the handle is an open NT handle of a shared fence, kept open by `ring`.
    unsafe { raw_device.OpenSharedHandle(nv12.handles.ready, &mut ready) }.map_err(|_| Fallback::ImportFailed)?;
    let ready = ready.ok_or(Fallback::ImportFailed)?;
    Ok(Exported { textures: Some(textures), ready, queue: raw_queue, wgpu_queue: queue.clone() })
}

impl Exported {
    /// The commands that copy `planes`, the luma (R8) and chroma (RG8) textures of one frame, into
    /// texture `slot`'s two planes. The planes must be in the copy-source state: the caller's last
    /// use of them, already recorded, is a copy from them. The NV12 texture starts and ends in the
    /// common state, the state another device's use of a shared texture needs.
    pub(crate) fn copy(&self, device: &wgpu::Device, planes: &[wgpu::Texture], slot: usize) -> Result<wgpu::CommandBuffer, Fallback> {
        use wgpu::hal::CommandEncoder as _;
        let dst = self.textures.as_ref().and_then(|t| t.get(slot)).ok_or(Fallback::ImportFailed)?;
        let (Some(luma), Some(chroma)) = (planes.first(), planes.get(1)) else { return Err(Fallback::ImportFailed) };
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("program to encoder") });
        // SAFETY: the guards are only used to name the plane textures in the copies below, and they
        // are dropped before the encoder is finished.
        let (Some(luma_hal), Some(chroma_hal)) =
            (unsafe { luma.as_hal::<wgpu::hal::api::Dx12>() }, unsafe { chroma.as_hal::<wgpu::hal::api::Dx12>() })
        else {
            return Err(Fallback::UnsupportedBackend);
        };
        let region = |aspect, width, height| wgpu::hal::TextureCopy {
            src_base: wgpu::hal::TextureCopyBase {
                mip_level: 0,
                array_layer: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::hal::FormatAspects::COLOR,
            },
            dst_base: wgpu::hal::TextureCopyBase { mip_level: 0, array_layer: 0, origin: wgpu::Origin3d::ZERO, aspect },
            size: wgpu::hal::CopyExtent { width, height, depth: 1 },
        };
        let barrier = |from, to| wgpu::hal::TextureBarrier {
            texture: dst,
            range: wgpu::ImageSubresourceRange::default(),
            usage: wgpu::hal::StateTransition { from, to },
        };
        let (luma_region, chroma_region) = (
            region(wgpu::hal::FormatAspects::PLANE_0, luma.width(), luma.height()),
            region(wgpu::hal::FormatAspects::PLANE_1, chroma.width(), chroma.height()),
        );
        // SAFETY: the encoder belongs to `device` and records nothing but these commands. The
        // plane textures are textures of `device` in the copy-source state, `dst` is open on the
        // same device, and each region lies inside both textures: `Exporter::copy` refuses planes
        // unless the luma plane is the NV12 texture's size and the chroma plane half of it. The
        // unused texture state (`UNINITIALIZED`) is the common state on Direct3D 12.
        let recorded = unsafe {
            encoder.as_hal_mut::<wgpu::hal::api::Dx12, _, _>(|hal| {
                let Some(hal) = hal else { return false };
                hal.transition_textures(std::iter::once(barrier(wgpu::TextureUses::UNINITIALIZED, wgpu::TextureUses::COPY_DST)));
                hal.copy_texture_to_texture(&*luma_hal, wgpu::TextureUses::COPY_SRC, dst, std::iter::once(luma_region));
                hal.copy_texture_to_texture(&*chroma_hal, wgpu::TextureUses::COPY_SRC, dst, std::iter::once(chroma_region));
                hal.transition_textures(std::iter::once(barrier(wgpu::TextureUses::COPY_DST, wgpu::TextureUses::UNINITIALIZED)));
                true
            })
        };
        drop((luma_hal, chroma_hal));
        if !recorded {
            return Err(Fallback::ImportFailed);
        }
        Ok(encoder.finish())
    }

    /// Set the ready fence to `value` after the work already submitted to the queue, the copy
    /// included.
    pub(crate) fn signal(&self, value: u64) -> Result<(), Fallback> {
        // SAFETY: `queue` is the device's queue, which is free-threaded, and `ready` is a fence
        // open on the same device.
        unsafe { self.queue.Signal(&self.ready, value) }.map_err(|_| Fallback::ImportFailed)
    }
}

impl Drop for Exported {
    fn drop(&mut self) {
        // A copy into the textures may still run: they are released once the queue has finished
        // the work submitted so far, without waiting here.
        let textures = self.textures.take();
        self.wgpu_queue.on_submitted_work_done(move || drop(textures));
    }
}
