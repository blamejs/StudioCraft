//! The compositor side on Windows: a ring's shared textures and fences opened on a wgpu Direct3D
//! 12 device. Every wgpu-hal and Direct3D 12 call of the import is here.

use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Device, ID3D12Fence, ID3D12Resource};

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
    let Some((raw_device, queue)) = raw(device) else { return Err(Fallback::NotDx12) };
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
    /// The ready fence's value now, without waiting.
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
