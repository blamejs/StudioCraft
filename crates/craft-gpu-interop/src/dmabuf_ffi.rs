//! The Linux system calls on a screen-cast stream's DMA-BUF descriptors (duplicating one, and
//! polling one for pending writes), and, with the `wgpu` feature, the Vulkan side of their import:
//! what the compositor's device can import, each buffer opened as a wgpu texture through wgpu-hal's
//! `texture_from_dmabuf_fd`, and the barriers that pass a buffer between the producer and the
//! compositor's queue. Every libc, wgpu-hal and Vulkan call of the DMA-BUF path is here.
//!
//! The export goes the other way: NV12 images of the compositor's Vulkan device, in a DRM format
//! modifier the device can share, whose memory is exported as DMA-BUFs for a hardware encoder. A
//! submission after each copy into an image signals a timeline semaphore with the frame's value,
//! which the encoder side reads without waiting ([`crate::ExportRing::ready`]). Each copy takes the
//! image from the external queue family, its old contents discarded, and hands it back in
//! `GENERAL`.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};

/// A new descriptor, closed on exec, for the open descriptor numbered `fd`, which stays its
/// owner's.
pub(crate) fn duplicate(fd: i32) -> std::io::Result<OwnedFd> {
    if fd < 0 {
        return Err(std::io::Error::from_raw_os_error(libc::EBADF));
    }
    // SAFETY: `F_DUPFD_CLOEXEC` only reads the descriptor number and does not change or close
    // `fd`; a number that is not open fails with `EBADF` and opens nothing.
    let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if new < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `new` is a descriptor the call above has just opened, and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(new) })
}

/// Whether `fd` polls readable now. For a DMA-BUF the kernel reports it once every write to the
/// buffer has finished. It does not wait.
pub(crate) fn readable(fd: BorrowedFd<'_>) -> bool {
    let mut entry = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: `entry` is one valid `pollfd` for a descriptor `fd` keeps open, the count is 1, and
    // a timeout of 0 returns at once.
    let n = unsafe { libc::poll(&mut entry, 1, 0) };
    n == 1 && entry.revents & libc::POLLIN != 0 && entry.revents & (libc::POLLERR | libc::POLLNVAL) == 0
}

/// Whether `fd` is a DMA-BUF: the kernel names the file of every DMA-BUF descriptor `/dmabuf:`
/// (`anon_inode:dmabuf` before Linux 5.3). Without `/proc` the answer is no.
#[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
pub(crate) fn is_dmabuf(fd: BorrowedFd<'_>) -> bool {
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd()))
        .is_ok_and(|target| target.to_str().is_some_and(|t| t.starts_with("/dmabuf:") || t == "anon_inode:dmabuf"))
}

/// Whether a plane `offset` bytes in, with rows `stride` bytes apart, can hold a `width` x `height`
/// picture of 4-byte pixels without the sizes overflowing; a linear one (`linear`) also needs rows
/// at least as long as the pixels.
#[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
pub(crate) fn plane_fits(width: u32, height: u32, offset: u32, stride: u32, linear: bool) -> bool {
    let row = u64::from(width) * 4;
    let stride = u64::from(stride);
    if width == 0 || height == 0 || stride == 0 || (linear && stride < row) {
        return false;
    }
    stride.checked_mul(u64::from(height)).and_then(|size| size.checked_add(u64::from(offset))).is_some()
}

/// The argument of `UDMABUF_CREATE` (`linux/udmabuf.h`).
#[repr(C)]
struct UdmabufCreate {
    memfd: u32,
    flags: u32,
    offset: u64,
    size: u64,
}

/// `_IOW('u', 0x42, struct udmabuf_create)`.
const UDMABUF_CREATE: u32 = (1 << 30) | ((std::mem::size_of::<UdmabufCreate>() as u32) << 16) | ((b'u' as u32) << 8) | 0x42;
const UDMABUF_FLAGS_CLOEXEC: u32 = 1;

fn check(result: libc::c_int) -> std::io::Result<libc::c_int> {
    if result < 0 { Err(std::io::Error::last_os_error()) } else { Ok(result) }
}

/// A DMA-BUF over `len` bytes of sealed anonymous memory, made through `/dev/udmabuf`. `len` must
/// be a whole number of pages. It fails where the kernel has no udmabuf device or the user cannot
/// open it.
pub(crate) fn dmabuf_in_memory(len: u64) -> std::io::Result<OwnedFd> {
    let size = libc::off_t::try_from(len).map_err(|_| std::io::Error::from_raw_os_error(libc::EINVAL))?;
    // SAFETY: the name is a NUL-terminated string that outlives the call, and the flags are valid.
    let memfd = check(unsafe { libc::memfd_create(c"craft-gpu-interop".as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) })?;
    // SAFETY: `memfd` is a descriptor the call above has just opened, and nothing else owns it.
    let memfd = unsafe { OwnedFd::from_raw_fd(memfd) };
    // SAFETY: `memfd` is open for the whole call, and `ftruncate` only sets its size.
    check(unsafe { libc::ftruncate(memfd.as_raw_fd(), size) })?;
    // SAFETY: `memfd` is open for the whole call; sealing it against shrinking is what udmabuf
    // requires, and the call changes nothing else.
    check(unsafe { libc::fcntl(memfd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK) })?;
    let device = std::fs::OpenOptions::new().read(true).write(true).open("/dev/udmabuf")?;
    let memfd_number = u32::try_from(memfd.as_raw_fd()).map_err(|_| std::io::Error::from_raw_os_error(libc::EBADF))?;
    let create = UdmabufCreate { memfd: memfd_number, flags: UDMABUF_FLAGS_CLOEXEC, offset: 0, size: len };
    // SAFETY: `device` and `memfd` are open for the whole call, and `create` is a valid
    // `udmabuf_create` that the kernel only reads during it.
    let fd = check(unsafe { libc::ioctl(device.as_raw_fd(), UDMABUF_CREATE as _, &create as *const UdmabufCreate) })?;
    // SAFETY: on success the ioctl returns a new descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(feature = "wgpu")]
pub(crate) use export::{Exported, export, export_backend};
#[cfg(all(feature = "wgpu", test))]
pub(crate) use export::{export_barriers, padding_regions};
#[cfg(feature = "wgpu")]
pub(crate) use vulkan::{Imported, caps, open};

#[cfg(feature = "wgpu")]
mod vulkan {
    use std::os::fd::AsFd;

    use ash::vk;

    use crate::dmabuf::{DmabufCaps, DmabufFormat, adapter_from_uuid, check_dmabuf};
    use crate::fallback::Fallback;
    use crate::ring::SharedRing;

    /// What `device` can import as DMA-BUFs. [`Fallback::UnsupportedBackend`] when it does not run
    /// on Vulkan, and [`Fallback::MissingExtension`] when it was opened without wgpu's
    /// `VULKAN_EXTERNAL_MEMORY_DMA_BUF` or runs a Vulkan older than 1.1.
    pub(crate) fn caps(device: &wgpu::Device) -> Result<DmabufCaps, Fallback> {
        // SAFETY: the guard is only used to read the physical device's properties, and it is
        // dropped before any other wgpu call on this device.
        let Some(hal) = (unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }) else { return Err(Fallback::UnsupportedBackend) };
        if !device.features().contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF) {
            return Err(Fallback::MissingExtension);
        }
        let instance = hal.shared_instance();
        if instance.instance_api_version() < vk::API_VERSION_1_1 {
            return Err(Fallback::MissingExtension);
        }
        let raw = instance.raw_instance();
        let physical = hal.raw_physical_device();
        let mut id = vk::PhysicalDeviceIDProperties::default();
        {
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
            // SAFETY: the physical device belongs to this instance, which runs Vulkan 1.1 or
            // later, and the call only fills `properties` and the `id` chained to it.
            unsafe { raw.get_physical_device_properties2(physical, &mut properties) };
        }
        let formats = DmabufFormat::ALL
            .into_iter()
            .map(|format| {
                let vk_format = if format.blue_first() { vk::Format::B8G8R8A8_UNORM } else { vk::Format::R8G8B8A8_UNORM };
                let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
                {
                    let mut properties = vk::FormatProperties2::default().push_next(&mut list);
                    // SAFETY: as above; with no array chained the call only reports the count.
                    unsafe { raw.get_physical_device_format_properties2(physical, vk_format, &mut properties) };
                }
                let count = usize::try_from(list.drm_format_modifier_count).unwrap_or(0).min(256);
                let mut entries = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
                {
                    let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut entries);
                    let mut properties = vk::FormatProperties2::default().push_next(&mut list);
                    // SAFETY: as above; the driver writes at most `count` entries into `entries`,
                    // the length the list was given.
                    unsafe { raw.get_physical_device_format_properties2(physical, vk_format, &mut properties) };
                }
                let modifiers = entries
                    .iter()
                    .filter(|e| {
                        e.drm_format_modifier_plane_count == 1
                            && e.drm_format_modifier_tiling_features.contains(vk::FormatFeatureFlags::SAMPLED_IMAGE)
                    })
                    .map(|e| e.drm_format_modifier)
                    .collect();
                (format, modifiers)
            })
            .collect();
        Ok(DmabufCaps { device: adapter_from_uuid(id.device_uuid), formats }.tidy())
    }

    /// A DMA-BUF ring opened on one Vulkan device: each slot's buffer as a wgpu texture once it has
    /// been imported.
    pub(crate) struct Imported {
        device: ash::Device,
        family: u32,
        pub(crate) textures: Vec<Option<wgpu::Texture>>,
        images: Vec<Option<vk::Image>>,
    }

    /// Open `ring` on `device`: the checks of [`check_dmabuf`], with no buffer imported yet.
    pub(crate) fn open(device: &wgpu::Device, ring: &SharedRing) -> Result<Imported, Fallback> {
        let caps = match caps(device) {
            Ok(c) => Some(c),
            Err(Fallback::UnsupportedBackend) => return Err(Fallback::UnsupportedBackend),
            Err(_) => None,
        };
        let Some(slots) = ring.dmabuf.as_deref() else { return Err(Fallback::ImportFailed) };
        check_dmabuf(true, caps.is_some(), caps.as_ref(), slots.device, slots.layout)?;
        // SAFETY: the guard is only used to clone the device's function table and read its queue
        // family, and it is dropped before the function returns.
        let Some(hal) = (unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }) else { return Err(Fallback::UnsupportedBackend) };
        let n = slots.planes.len();
        Ok(Imported { device: hal.raw_device().clone(), family: hal.queue_family_index(), textures: vec![None; n], images: vec![None; n] })
    }

    impl Imported {
        /// Import the buffer in `slot` of `ring`, once. [`Fallback::ImportFailed`] when the slot has
        /// no plane or the driver refuses the buffer.
        pub(crate) fn import(&mut self, device: &wgpu::Device, ring: &SharedRing, slot: usize) -> Result<&wgpu::Texture, Fallback> {
            if self.textures.get(slot).is_some_and(Option::is_some) {
                return self.textures.get(slot).and_then(Option::as_ref).ok_or(Fallback::ImportFailed);
            }
            let (Some(slots), Some(plane)) = (ring.dmabuf.as_deref(), ring.plane(slot)) else { return Err(Fallback::ImportFailed) };
            // The driver is handed only a DMA-BUF whose stride and offset describe the picture.
            let linear = slots.layout.modifier == crate::dmabuf::MOD_LINEAR;
            if !super::plane_fits(ring.width(), ring.height(), plane.offset(), plane.stride(), linear) || !super::is_dmabuf(plane.fd.as_fd()) {
                return Err(Fallback::ImportFailed);
            }
            let format = slots.layout.format.texture_format();
            let size = wgpu::Extent3d { width: ring.width(), height: ring.height(), depth_or_array_layers: 1 };
            let desc = wgpu::TextureDescriptor {
                label: Some("screen-cast buffer"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            };
            let hal_desc = wgpu::hal::TextureDescriptor {
                label: desc.label,
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: desc.dimension,
                format,
                usage: wgpu::TextureUses::RESOURCE,
                memory_flags: wgpu::hal::MemoryFlags::empty(),
                view_formats: Vec::new(),
            };
            // The import takes the descriptor it is given, so it gets a duplicate; the ring keeps
            // its own.
            let fd = plane.fd.try_clone().map_err(|_| Fallback::ImportFailed)?;
            let texture = {
                // SAFETY: the guard is only used to import the buffer, and it is dropped before the
                // texture is handed to wgpu.
                let Some(hal) = (unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }) else { return Err(Fallback::UnsupportedBackend) };
                // SAFETY: the device has `VULKAN_EXTERNAL_MEMORY_DMA_BUF` (`open` checked it), `fd`
                // is a DMA-BUF (`is_dmabuf` checked it) that the stream negotiated as a single-plane
                // `size` picture in `format` with the ring's modifier, which `check_dmabuf` found
                // among those the device can sample, and the stride and offset are the ones the
                // stream reported for it, checked by `plane_fits`. The call takes `fd`.
                unsafe { hal.texture_from_dmabuf_fd(fd, &hal_desc, slots.layout.modifier, u64::from(plane.stride()), u64::from(plane.offset())) }
                    .map_err(|_| Fallback::ImportFailed)?
            };
            // SAFETY: `texture` was made on this device and matches `desc`. wgpu is told the image is
            // in `SHADER_READ_ONLY_OPTIMAL` (`RESOURCE`), the layout it has at every draw: each draw's
            // first barrier moves it there from `GENERAL`, the layout the producer writes it in, and
            // wgpu records no layout change of its own for a texture that is only sampled. wgpu does
            // not clear a texture made from wgpu-hal.
            let texture = unsafe { device.create_texture_from_hal::<wgpu::hal::api::Vulkan>(texture, &desc, wgpu::TextureUses::RESOURCE) };
            // SAFETY: the guard is only used to read the image handle, which stays owned by wgpu.
            let image = unsafe { texture.as_hal::<wgpu::hal::api::Vulkan>().map(|t| t.raw_handle()) }.ok_or(Fallback::ImportFailed)?;
            let (Some(t), Some(i)) = (self.textures.get_mut(slot), self.images.get_mut(slot)) else { return Err(Fallback::ImportFailed) };
            *i = Some(image);
            Ok(t.insert(texture))
        }

        /// The commands that pass the buffer in `slot` from the producer to this queue before the
        /// draw that reads it, and back after.
        pub(crate) fn acquire(&self, device: &wgpu::Device, slot: usize) -> Option<wgpu::CommandBuffer> {
            self.barrier(device, slot, true)
        }

        pub(crate) fn hand_back(&self, device: &wgpu::Device, slot: usize) -> Option<wgpu::CommandBuffer> {
            self.barrier(device, slot, false)
        }

        fn barrier(&self, wgpu_device: &wgpu::Device, slot: usize, acquire: bool) -> Option<wgpu::CommandBuffer> {
            let image = (*self.images.get(slot)?)?;
            let (from, to) = if acquire { (vk::QUEUE_FAMILY_EXTERNAL, self.family) } else { (self.family, vk::QUEUE_FAMILY_EXTERNAL) };
            let (src_access, dst_access) = if acquire {
                (vk::AccessFlags::empty(), vk::AccessFlags::SHADER_READ)
            } else {
                (vk::AccessFlags::SHADER_READ, vk::AccessFlags::empty())
            };
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
            let device = &self.device;
            let mut encoder = wgpu_device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("screen-cast buffer ownership") });
            // SAFETY: the encoder belongs to `wgpu_device` and records nothing but the barrier; the
            // command buffer handle is only used while the closure runs, and the image is an image
            // of that device. wgpu does not let an encoder that recorded raw commands record its
            // own, so the barrier is a command buffer of its own.
            unsafe {
                encoder.as_hal_mut::<wgpu::hal::api::Vulkan, _, _>(|hal| {
                    if let Some(hal) = hal {
                        device.cmd_pipeline_barrier(hal.raw_handle(), src_stage, dst_stage, vk::DependencyFlags::empty(), &[], &[], &[barrier]);
                    }
                })
            };
            Some(encoder.finish())
        }
    }
}

#[cfg(feature = "wgpu")]
mod export {
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use ash::vk;

    use crate::dmabuf::adapter_from_uuid;
    use crate::export::{ExportBuffer, ExportRing, ReadyCounter, dev_number};
    use crate::fallback::{Backend, Fallback, check_dmabuf_export};
    use crate::ring::{AdapterId, SLOTS};

    /// The Vulkan format of the export ring's images: an 8-bit luma plane and an interleaved
    /// chroma plane of half the size, NV12.
    const NV12: vk::Format = vk::Format::G8_B8R8_2PLANE_420_UNORM;

    /// Most modifiers read from a driver for the NV12 format.
    const MAX_MODIFIERS: usize = 64;

    /// What `device`'s driver reports for an export: its adapter, whether it has timeline semaphores
    /// whose value the CPU can read (Vulkan 1.2), and whether the device was opened with
    /// `VK_KHR_external_memory_fd`, `VK_EXT_external_memory_dma_buf` and
    /// `VK_EXT_image_drm_format_modifier`. `None` when the device does not run on Vulkan.
    pub(crate) fn export_backend(device: &wgpu::Device) -> Option<(Option<AdapterId>, bool, bool)> {
        // SAFETY: the guard is only used to read the physical device's properties and features and
        // the device's extensions, and it is dropped before any other wgpu call on this device.
        let hal = unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }?;
        let instance = hal.shared_instance();
        if instance.instance_api_version() < vk::API_VERSION_1_1 {
            return Some((None, false, false));
        }
        let raw = instance.raw_instance();
        let physical = hal.raw_physical_device();
        let mut id = vk::PhysicalDeviceIDProperties::default();
        let api = {
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut id);
            // SAFETY: the physical device belongs to this instance, which runs Vulkan 1.1 or later,
            // and the call only fills `properties` and the `id` chained to it.
            unsafe { raw.get_physical_device_properties2(physical, &mut properties) };
            properties.properties.api_version
        };
        let adapter = Some(adapter_from_uuid(id.device_uuid));
        let enabled = hal.enabled_device_extensions();
        let dmabuf = [ash::khr::external_memory_fd::NAME, ash::ext::external_memory_dma_buf::NAME, ash::ext::image_drm_format_modifier::NAME]
            .iter()
            .all(|name| enabled.contains(name));
        // Reading a timeline semaphore's value from the CPU is a Vulkan 1.2 call.
        if api < vk::API_VERSION_1_2 || instance.instance_api_version() < vk::API_VERSION_1_2 {
            return Some((adapter, false, dmabuf));
        }
        let mut timeline = vk::PhysicalDeviceTimelineSemaphoreFeatures::default();
        {
            let mut features = vk::PhysicalDeviceFeatures2::default().push_next(&mut timeline);
            // SAFETY: as above; the call only fills `features` and the `timeline` chained to it.
            unsafe { raw.get_physical_device_features2(physical, &mut features) };
        }
        Some((adapter, timeline.timeline_semaphore == vk::TRUE, dmabuf))
    }

    /// The DRM format modifiers in which `physical` can make a `width` x `height` NV12 image to copy
    /// into and export as a DMA-BUF: two memory planes (no planes of compression data, which an
    /// encoder would have to read too), transfer writes, and memory that can be exported.
    fn nv12_modifiers(raw: &ash::Instance, physical: vk::PhysicalDevice, width: u32, height: u32) -> Vec<u64> {
        let mut list = vk::DrmFormatModifierPropertiesListEXT::default();
        {
            let mut properties = vk::FormatProperties2::default().push_next(&mut list);
            // SAFETY: the physical device belongs to this instance, which runs Vulkan 1.1 or later;
            // with no array chained the call only reports the count.
            unsafe { raw.get_physical_device_format_properties2(physical, NV12, &mut properties) };
        }
        let count = usize::try_from(list.drm_format_modifier_count).unwrap_or(0).min(MAX_MODIFIERS);
        let mut entries = vec![vk::DrmFormatModifierPropertiesEXT::default(); count];
        {
            let mut list = vk::DrmFormatModifierPropertiesListEXT::default().drm_format_modifier_properties(&mut entries);
            let mut properties = vk::FormatProperties2::default().push_next(&mut list);
            // SAFETY: as above; the driver writes at most `count` entries into `entries`, the length
            // the list was given.
            unsafe { raw.get_physical_device_format_properties2(physical, NV12, &mut properties) };
        }
        entries
            .iter()
            .filter(|e| {
                e.drm_format_modifier_plane_count == 2 && e.drm_format_modifier_tiling_features.contains(vk::FormatFeatureFlags::TRANSFER_DST)
            })
            .map(|e| e.drm_format_modifier)
            .filter(|&modifier| {
                let mut drm =
                    vk::PhysicalDeviceImageDrmFormatModifierInfoEXT::default().drm_format_modifier(modifier).sharing_mode(vk::SharingMode::EXCLUSIVE);
                let mut external = vk::PhysicalDeviceExternalImageFormatInfo::default().handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
                let info = vk::PhysicalDeviceImageFormatInfo2::default()
                    .format(NV12)
                    .ty(vk::ImageType::TYPE_2D)
                    .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                    .usage(vk::ImageUsageFlags::TRANSFER_DST)
                    .push_next(&mut external)
                    .push_next(&mut drm);
                let mut exported = vk::ExternalImageFormatProperties::default();
                let mut properties = vk::ImageFormatProperties2::default().push_next(&mut exported);
                // SAFETY: as above; `info` and its chain describe one image, and the call only
                // fills `properties` and the `exported` chained to it.
                let supported = unsafe { raw.get_physical_device_image_format_properties2(physical, &info, &mut properties) }.is_ok();
                let max = properties.image_format_properties.max_extent;
                supported
                    && max.width >= width
                    && max.height >= height
                    && exported.external_memory_properties.external_memory_features.contains(vk::ExternalMemoryFeatureFlags::EXPORTABLE)
            })
            .collect()
    }

    /// The device number of the DRM render node of `physical`, where the driver reports it through
    /// `VK_EXT_physical_device_drm`.
    fn render_node(raw: &ash::Instance, physical: vk::PhysicalDevice) -> Option<u64> {
        // SAFETY: the physical device belongs to this instance; the call only lists its extensions.
        let extensions = unsafe { raw.enumerate_device_extension_properties(physical) }.ok()?;
        if !extensions.iter().any(|e| e.extension_name_as_c_str() == Ok(ash::ext::physical_device_drm::NAME)) {
            return None;
        }
        let mut drm = vk::PhysicalDeviceDrmPropertiesEXT::default();
        {
            let mut properties = vk::PhysicalDeviceProperties2::default().push_next(&mut drm);
            // SAFETY: the device supports `VK_EXT_physical_device_drm`, whose properties need only
            // its support, and the call only fills `properties` and the `drm` chained to it.
            unsafe { raw.get_physical_device_properties2(physical, &mut properties) };
        }
        if drm.has_render != vk::TRUE {
            return None;
        }
        Some(dev_number(u32::try_from(drm.render_major).ok()?, u32::try_from(drm.render_minor).ok()?))
    }

    /// One NV12 image of the ring and the memory bound to it.
    struct Image {
        image: vk::Image,
        memory: vk::DeviceMemory,
    }

    /// The Vulkan objects of an export and what makes more of them.
    struct Maker<'a> {
        instance: &'a ash::Instance,
        physical: vk::PhysicalDevice,
        device: &'a ash::Device,
        width: u32,
        height: u32,
        modifiers: &'a [u64],
    }

    impl Maker<'_> {
        /// A memory type for `bits`: device-local where one is allowed.
        fn memory_type(&self, bits: u32) -> Option<u32> {
            // SAFETY: the physical device belongs to this instance; the call only reads properties.
            let properties = unsafe { self.instance.get_physical_device_memory_properties(self.physical) };
            let count = usize::try_from(properties.memory_type_count).unwrap_or(0).min(properties.memory_types.len());
            let allowed = |i: usize| bits & (1 << i) != 0;
            let local = (0..count).find(|&i| allowed(i) && properties.memory_types[i].property_flags.contains(vk::MemoryPropertyFlags::DEVICE_LOCAL));
            local.or_else(|| (0..count).find(|&i| allowed(i))).and_then(|i| u32::try_from(i).ok())
        }

        /// One NV12 image in one of the modifiers, with its memory, exported as a DMA-BUF. Whatever
        /// was made is destroyed again on failure.
        fn make(&self) -> Result<(Image, ExportBuffer), Fallback> {
            let mut list = vk::ImageDrmFormatModifierListCreateInfoEXT::default().drm_format_modifiers(self.modifiers);
            let mut external = vk::ExternalMemoryImageCreateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let info = vk::ImageCreateInfo::default()
                .image_type(vk::ImageType::TYPE_2D)
                .format(NV12)
                .extent(vk::Extent3D { width: self.width, height: self.height, depth: 1 })
                .mip_levels(1)
                .array_layers(1)
                .samples(vk::SampleCountFlags::TYPE_1)
                .tiling(vk::ImageTiling::DRM_FORMAT_MODIFIER_EXT)
                .usage(vk::ImageUsageFlags::TRANSFER_DST)
                .sharing_mode(vk::SharingMode::EXCLUSIVE)
                .initial_layout(vk::ImageLayout::UNDEFINED)
                .push_next(&mut external)
                .push_next(&mut list);
            // SAFETY: the device was opened with the DMA-BUF and modifier extensions
            // (`export_backend`), and `info` describes a single-sampled 2D NV12 image in modifiers
            // the device reported for that format, size, usage and exportable memory.
            let image = unsafe { self.device.create_image(&info, None) }.map_err(|_| Fallback::ImportFailed)?;
            let made = self.bind(image);
            if made.is_err() {
                // SAFETY: no command has used the image, and `bind` freed any memory it allocated.
                unsafe { self.device.destroy_image(image, None) };
            }
            made
        }

        /// Allocate exportable memory for `image`, bind it, and export it as a DMA-BUF. The memory
        /// is freed again on failure; the image is the caller's.
        fn bind(&self, image: vk::Image) -> Result<(Image, ExportBuffer), Fallback> {
            // SAFETY: `image` is an image of this device without memory bound.
            let requirements = unsafe { self.device.get_image_memory_requirements(image) };
            let kind = self.memory_type(requirements.memory_type_bits).ok_or(Fallback::ImportFailed)?;
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().image(image);
            let mut export = vk::ExportMemoryAllocateInfo::default().handle_types(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            let allocate = vk::MemoryAllocateInfo::default()
                .allocation_size(requirements.size)
                .memory_type_index(kind)
                .push_next(&mut export)
                .push_next(&mut dedicated);
            // SAFETY: the allocation is dedicated to `image`, of the size and a type its requirements
            // allow, and exportable as a DMA-BUF, which the image was made for.
            let memory = unsafe { self.device.allocate_memory(&allocate, None) }.map_err(|_| Fallback::ImportFailed)?;
            let exported = self.export(image, memory, requirements.size);
            if exported.is_err() {
                // SAFETY: no command has used the memory.
                unsafe { self.device.free_memory(memory, None) };
            }
            exported.map(|buffer| (Image { image, memory }, buffer))
        }

        fn export(&self, image: vk::Image, memory: vk::DeviceMemory, size: u64) -> Result<ExportBuffer, Fallback> {
            // SAFETY: `memory` was allocated for `image` above, at offset 0.
            unsafe { self.device.bind_image_memory(image, memory, 0) }.map_err(|_| Fallback::ImportFailed)?;
            let modifiers = ash::ext::image_drm_format_modifier::Device::new(self.instance, self.device);
            let mut properties = vk::ImageDrmFormatModifierPropertiesEXT::default();
            // SAFETY: `image` was made with `DRM_FORMAT_MODIFIER_EXT` tiling on a device with the
            // extension, and the call only fills `properties`.
            unsafe { modifiers.get_image_drm_format_modifier_properties(image, &mut properties) }.map_err(|_| Fallback::ImportFailed)?;
            let layout = |aspect| {
                let sub = vk::ImageSubresource { aspect_mask: aspect, mip_level: 0, array_layer: 0 };
                // SAFETY: `image` has `DRM_FORMAT_MODIFIER_EXT` tiling and its modifier has two memory
                // planes (`nv12_modifiers`), so memory planes 0 and 1 name planes of it.
                let l = unsafe { self.device.get_image_subresource_layout(image, sub) };
                Some((u32::try_from(l.offset).ok()?, u32::try_from(l.row_pitch).ok()?))
            };
            let (Some((o0, p0)), Some((o1, p1))) =
                (layout(vk::ImageAspectFlags::MEMORY_PLANE_0_EXT), layout(vk::ImageAspectFlags::MEMORY_PLANE_1_EXT))
            else {
                return Err(Fallback::ImportFailed);
            };
            let fds = ash::khr::external_memory_fd::Device::new(self.instance, self.device);
            let info = vk::MemoryGetFdInfoKHR::default().memory(memory).handle_type(vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT);
            // SAFETY: `memory` was allocated exportable as a DMA-BUF on a device with
            // `VK_KHR_external_memory_fd`; each call returns a new descriptor the caller owns.
            let fd = unsafe { fds.get_memory_fd(&info) }.map_err(|_| Fallback::ImportFailed)?;
            if fd < 0 {
                return Err(Fallback::ImportFailed);
            }
            // SAFETY: `fd` is a descriptor the call above has just opened, and nothing else owns it.
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            ExportBuffer::new(fd, size, properties.drm_format_modifier, [o0, o1], [p0, p1], (self.width, self.height)).ok_or(Fallback::ImportFailed)
        }
    }

    /// An export ring's NV12 images on the compositor's Vulkan device and the semaphore its ready
    /// value comes from.
    pub(crate) struct Exported {
        images: Vec<Image>,
        family: u32,
        /// The ring's size; the images are that size rounded up to whole 16 x 16 blocks.
        size: (u32, u32),
        /// The copies that fill each plane's padding ([`padding_regions`]), made once when the ring
        /// opens so a frame's copy allocates nothing.
        luma_padding: Vec<vk::ImageCopy>,
        chroma_padding: Vec<vk::ImageCopy>,
        device: ash::Device,
        wgpu_device: wgpu::Device,
        queue: wgpu::Queue,
        ready: Arc<Ready>,
    }

    /// Make an export ring of `width` x `height` DMA-BUFs on `device`, whose queue is `queue`.
    pub(crate) fn export(device: &wgpu::Device, queue: &wgpu::Queue, width: u32, height: u32) -> Result<(Arc<ExportRing>, Exported), Fallback> {
        let Some((adapter, timeline, dmabuf)) = export_backend(device) else { return Err(Fallback::UnsupportedBackend) };
        let (instance, physical, vk_device, family) = {
            // SAFETY: the guard is only used to clone the instance's and the device's function tables
            // and read the physical device and queue family, and it is dropped before any other wgpu
            // call on this device. The clones are used only while `wgpu_device`, kept in `Exported`,
            // keeps the device.
            let Some(hal) = (unsafe { device.as_hal::<wgpu::hal::api::Vulkan>() }) else { return Err(Fallback::UnsupportedBackend) };
            (hal.shared_instance().raw_instance().clone(), hal.raw_physical_device(), hal.raw_device().clone(), hal.queue_family_index())
        };
        // The images hold whole 16 x 16 blocks; the copies write the picture at their top left.
        let (iw, ih) = ExportRing::padded_size(width, height).ok_or(Fallback::ImportFailed)?;
        let modifiers = if dmabuf { nv12_modifiers(&instance, physical, iw, ih) } else { Vec::new() };
        let adapter = check_dmabuf_export(Backend::Vulkan { external_memory: dmabuf, timeline }, adapter, &modifiers)?;
        let maker = Maker { instance: &instance, physical, device: &vk_device, width: iw, height: ih, modifiers: &modifiers };
        let mut images = Vec::with_capacity(SLOTS);
        let mut buffers = Vec::with_capacity(SLOTS);
        for _ in 0..SLOTS {
            match maker.make() {
                Ok((image, buffer)) => {
                    images.push(image);
                    buffers.push(buffer);
                }
                Err(reason) => {
                    destroy(&vk_device, images);
                    return Err(reason);
                }
            }
        }
        let ring = Arc::new(ExportRing::of_dmabufs(adapter, width, height, buffers, render_node(&instance, physical)));
        let ready = match Ready::new(vk_device.clone(), queue.clone(), Arc::clone(&ring)) {
            Ok(ready) => Arc::new(ready),
            Err(reason) => {
                destroy(&vk_device, images);
                return Err(reason);
            }
        };
        let weak: std::sync::Weak<Ready> = Arc::downgrade(&ready);
        ring.count_ready_with(weak);
        let luma_padding = padding_regions((width, height), (iw, ih), vk::ImageAspectFlags::PLANE_0);
        let chroma_padding = padding_regions((width / 2, height / 2), (iw / 2, ih / 2), vk::ImageAspectFlags::PLANE_1);
        Ok((
            ring,
            Exported {
                images,
                family,
                size: (width, height),
                luma_padding,
                chroma_padding,
                device: vk_device,
                wgpu_device: device.clone(),
                queue: queue.clone(),
                ready,
            },
        ))
    }

    /// The copies that fill the padding of a plane of `dst` texels from a source plane of `src`
    /// texels at its top left: the source's last column into each column past it, its last row into
    /// each row past it, and its corner texel into each texel of the corner. The padding then
    /// repeats the picture's edge, as a frame copied through memory does, so the encoder codes the
    /// edge blocks the same way. None when the source fills the plane.
    pub(crate) fn padding_regions(src: (u32, u32), dst: (u32, u32), aspect: vk::ImageAspectFlags) -> Vec<vk::ImageCopy> {
        let ((w, h), (dw, dh)) = (src, dst);
        if w == 0 || h == 0 {
            return Vec::new();
        }
        let at = |x: u32, y: u32| vk::Offset3D { x: i32::try_from(x).unwrap_or(i32::MAX), y: i32::try_from(y).unwrap_or(i32::MAX), z: 0 };
        let layers = |aspect| vk::ImageSubresourceLayers { aspect_mask: aspect, mip_level: 0, base_array_layer: 0, layer_count: 1 };
        let copy = |from: (u32, u32), to: (u32, u32), extent: (u32, u32)| vk::ImageCopy {
            src_subresource: layers(vk::ImageAspectFlags::COLOR),
            src_offset: at(from.0, from.1),
            dst_subresource: layers(aspect),
            dst_offset: at(to.0, to.1),
            extent: vk::Extent3D { width: extent.0, height: extent.1, depth: 1 },
        };
        let mut regions = Vec::new();
        for x in w..dw {
            regions.push(copy((w - 1, 0), (x, 0), (1, h)));
        }
        for y in h..dh {
            regions.push(copy((0, h - 1), (0, y), (w, 1)));
            for x in w..dw {
                regions.push(copy((w - 1, h - 1), (x, y), (1, 1)));
            }
        }
        regions
    }

    /// Destroy `images` and free their memory.
    fn destroy(device: &ash::Device, images: Vec<Image>) {
        for Image { image, memory } in images {
            // SAFETY: no submitted work uses the image or its memory any more, and nothing else
            // refers to them; the image goes before its memory.
            unsafe {
                device.destroy_image(image, None);
                device.free_memory(memory, None);
            }
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

    impl Exported {
        /// The commands that copy `planes`, the luma (R8) and chroma (RG8) textures of one frame,
        /// into image `slot`'s two planes. The planes must be in `TRANSFER_SRC_OPTIMAL`: the
        /// caller's last use of them, already recorded, is a copy from them.
        pub(crate) fn copy(&self, planes: &[wgpu::Texture], slot: usize) -> Result<wgpu::CommandBuffer, Fallback> {
            let dst = self.images.get(slot).ok_or(Fallback::ImportFailed)?.image;
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
            let (w, h) = self.size;
            if (luma.width(), luma.height()) != (w, h) || (chroma.width(), chroma.height()) != (w / 2, h / 2) {
                return Err(Fallback::ImportFailed);
            }
            let (luma_region, chroma_region) = (region(vk::ImageAspectFlags::PLANE_0, luma), region(vk::ImageAspectFlags::PLANE_1, chroma));
            let device = &self.device;
            let (luma_padding, chroma_padding) = (&self.luma_padding, &self.chroma_padding);
            let mut encoder = self.wgpu_device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("program to encoder") });
            // SAFETY: the encoder belongs to `wgpu_device` and records nothing but these commands,
            // and the command buffer handle is only used while the closure runs. The plane images
            // are images of the device in `TRANSFER_SRC_OPTIMAL`, `dst` is an NV12 image of the
            // device, and each region lies inside both images: the planes have the ring's size
            // (checked above), which the NV12 image's padded planes hold, and the padding regions,
            // made for that size when the ring opened, read only texels of the planes and write
            // only texels of the padded planes. The barriers move
            // `dst` from the encoder side to this queue and back around the copies. wgpu does not let an
            // encoder that recorded raw commands record its own, so they are a command buffer of
            // their own.
            unsafe {
                encoder.as_hal_mut::<wgpu::hal::api::Vulkan, _, _>(|hal| {
                    let Some(hal) = hal else { return };
                    let cmd = hal.raw_handle();
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
                    // The padding repeats the picture's edge; a size of whole blocks has none.
                    if !luma_padding.is_empty() {
                        device.cmd_copy_image(
                            cmd,
                            luma_image,
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            dst,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            luma_padding,
                        );
                    }
                    if !chroma_padding.is_empty() {
                        device.cmd_copy_image(
                            cmd,
                            chroma_image,
                            vk::ImageLayout::TRANSFER_SRC_OPTIMAL,
                            dst,
                            vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                            chroma_padding,
                        );
                    }
                    device.cmd_pipeline_barrier(
                        cmd,
                        vk::PipelineStageFlags::TRANSFER,
                        vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                        vk::DependencyFlags::empty(),
                        &[],
                        &[],
                        &[hand_back],
                    );
                })
            };
            Ok(encoder.finish())
        }

        /// Set the ready value to `value` once the work already submitted to the queue, the copy
        /// included, has finished.
        pub(crate) fn signal(&self, value: u64) {
            self.ready.after_submitted(value);
        }

        /// Copy the value the queue has reached to the ring, without waiting.
        pub(crate) fn poll(&self) {
            self.ready.poll();
        }
    }

    impl Drop for Exported {
        fn drop(&mut self) {
            // A copy into the images may still run: they are destroyed once the queue has finished
            // the work submitted so far, without waiting here. The callback keeps the wgpu device,
            // and with it the Vulkan device, until then.
            let images = std::mem::take(&mut self.images);
            let (device, wgpu_device) = (self.device.clone(), self.wgpu_device.clone());
            self.queue.on_submitted_work_done(move || {
                destroy(&device, images);
                drop(wgpu_device);
            });
        }
    }

    /// A timeline semaphore that the compositor's queue signals after each copy with the frame's
    /// value. The ring reads its value without waiting, and [`Ready::poll`] copies it to the ring.
    struct Ready {
        device: ash::Device,
        queue: wgpu::Queue,
        semaphore: vk::Semaphore,
        ring: Arc<ExportRing>,
        /// The newest value queued on `semaphore`.
        queued: AtomicU64,
        /// The newest value on `semaphore` that went out with a submission, apart from `queued`.
        submitted: AtomicU64,
    }

    impl Ready {
        /// A semaphore of `device` that starts at the ring's ready value. The device must have
        /// timeline semaphores.
        fn new(device: ash::Device, queue: wgpu::Queue, ring: Arc<ExportRing>) -> Result<Ready, Fallback> {
            let start = ring.ready();
            let mut kind = vk::SemaphoreTypeCreateInfo::default().semaphore_type(vk::SemaphoreType::TIMELINE).initial_value(start);
            let info = vk::SemaphoreCreateInfo::default().push_next(&mut kind);
            // SAFETY: the device has timeline semaphores (its caller checked) and `info` describes one.
            let semaphore = unsafe { device.create_semaphore(&info, None) }.map_err(|_| Fallback::ImportFailed)?;
            Ok(Ready { device, queue, semaphore, ring, queued: AtomicU64::new(start), submitted: AtomicU64::new(start) })
        }

        /// Signal the semaphore with `value` once the work already submitted to the queue has
        /// finished. It is called after that submission: an empty submission signals the semaphore.
        /// A signal is applied to whichever submission comes next on the queue, from any thread, and
        /// every such submission comes after the work, so its signal also covers the work.
        fn after_submitted(&self, value: u64) {
            if value <= self.queued.load(Ordering::Acquire) {
                return;
            }
            // SAFETY: the guard is only used to change the semaphores the next submission signals,
            // and it is dropped before any other wgpu call on this queue.
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

        /// Copy the value the queue has signaled on the semaphore to the ring, without waiting.
        fn poll(&self) {
            if let Some(done) = self.reached() {
                self.ring.set_ready(done);
            }
        }
    }

    impl ReadyCounter for Ready {
        fn reached(&self) -> Option<u64> {
            // SAFETY: `semaphore` is a timeline semaphore of this device, which runs Vulkan 1.2;
            // reading its value does not wait.
            unsafe { self.device.get_semaphore_counter_value(self.semaphore) }.ok()
        }
    }

    impl Drop for Ready {
        fn drop(&mut self) {
            // SAFETY: as in `after_submitted`; a signal not submitted yet is taken off the list.
            let unsubmitted = unsafe { self.queue.as_hal::<wgpu::hal::api::Vulkan>() }.is_some_and(|hal| hal.remove_signal_semaphore(self.semaphore));
            // The newest value a submission still signals.
            let target = if unsubmitted { self.submitted.load(Ordering::Acquire) } else { self.queued.load(Ordering::Acquire) };
            self.poll();
            if self.reached().is_some_and(|done| done >= target) {
                // SAFETY: no submission still signals the semaphore: the one not submitted was taken
                // off the list, and the submitted ones have finished.
                unsafe { self.device.destroy_semaphore(self.semaphore, None) };
                return;
            }
            // Work still runs: once the queue has finished the work submitted so far, the ring gets
            // the last value and the semaphore is destroyed, without waiting here.
            let (device, semaphore, ring) = (self.device.clone(), self.semaphore, Arc::clone(&self.ring));
            self.queue.on_submitted_work_done(move || {
                ring.set_ready(target);
                // SAFETY: every submission that signals the semaphore was made before this callback
                // was registered, so all of them have finished, and nothing uses it afterward.
                unsafe { device.destroy_semaphore(semaphore, None) };
            });
        }
    }
}
