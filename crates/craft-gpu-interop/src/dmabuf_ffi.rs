//! The Linux system calls on a screen-cast stream's DMA-BUF descriptors (duplicating one, and
//! polling one for pending writes), and, with the `wgpu` feature, the Vulkan side of their import:
//! what the compositor's device can import, each buffer opened as a wgpu texture through wgpu-hal's
//! `texture_from_dmabuf_fd`, and the barriers that pass a buffer between the producer and the
//! compositor's queue. Every libc, wgpu-hal and Vulkan call of the DMA-BUF path is here.

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
