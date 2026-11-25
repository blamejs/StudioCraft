# craft-gpu-interop

craft-gpu-interop hands captured display and window pictures from the capture side to a wgpu
compositor without copying them through CPU memory. On Windows it captures a display with DXGI
desktop duplication, or a window with Windows.Graphics.Capture, into Direct3D 11 textures that are
shared through NT handles, and opens those textures and two shared fences on a wgpu Direct3D 12 or
Vulkan device. On Linux it takes the DMA-BUF buffers of a display's screen-cast stream and opens
each one as a texture on a wgpu Vulkan device with `VK_EXT_external_memory_dma_buf` and
`VK_EXT_image_drm_format_modifier`. Without the `wgpu` feature the crate
builds for every target, WebAssembly included, and holds the types both sides exchange.

It also hands the compositor's NV12 program frames to a hardware encoder in the other direction:
an export ring of shared NV12 textures and two fences on a Direct3D 11 device of the compositor's
adapter, written by the compositor's wgpu Direct3D 12 or Vulkan device and read by the encoder.
On Linux the export ring's images are NV12 images on the compositor's Vulkan device whose memory
is exported as DMA-BUFs, for a VA-API encoder on the same GPU's render node.

Only the private `dxgi_ffi` and `wgc_ffi` (capture side), `d3d12_ffi` and `vulkan_ffi`
(compositor side), `dmabuf_ffi` (Linux, both sides) and `pw_hold_ffi` (Linux, capture side, with the
`pipewire` feature) modules contain `unsafe` code. They call Direct3D and Windows.Graphics.Capture
through the `windows` crate's bindings, Vulkan through `ash`, wgpu through `wgpu::hal`, the Linux
system calls through `libc`, and PipeWire's buffer queue through the `pipewire` crate.

## Contents

| Item | Description |
|---|---|
| `Duplication::open(device_name)` | It opens desktop duplication of the output with that GDI device name (`\\.\DISPLAY1`) and a ring of `SLOTS` (3) shared BGRA textures. Elsewhere than Windows it returns `CaptureError::Fallback(Fallback::Unsupported)`. |
| `Duplication::grab()` | It returns at once: `Grab::Frame` with a new picture, or `Grab::Unchanged`. `CaptureError::Lost` means the desktop changed mode or was switched and the duplication must be opened again; `CaptureError::Fallback` means the pictures must go through CPU memory. |
| `WindowCapture::open(window, display)` | It opens Windows.Graphics.Capture of the window with that handle (its `HWND` as a number) on a Direct3D 11 device of the adapter that drives the display with that GDI device name, the display the window is on, and a ring of `SLOTS` shared BGRA textures of the window's size. An older Windows without window capture into textures returns `Fallback::Unsupported`, a window the system does not let be captured that way returns `Fallback::Refused`, and a window that closed before the call returns `CaptureError::Lost`. Elsewhere than Windows it returns `Fallback::Unsupported`. |
| `WindowCapture::grab()` | It returns at once: `Grab::Frame` with a new picture, or `Grab::Unchanged`. When the window's size changes, the capture makes the system's buffers that size and returns `Grab::Unchanged`, and the next picture goes into a new ring of the new size (`ring`). A minimized window gives `Grab::Unchanged`. `CaptureError::Lost` means the window was closed; `CaptureError::Fallback` means the pictures must go through CPU memory. |
| `SharedRing` | It holds one capture's shared handles, its size, the adapter that owns the textures, and the reason the compositor gave for refusing them (`fallback`, `refuse`). `SharedRing::detached` makes a ring without textures for tests. |
| `SharedFrame` | It names one texture of a ring and the ready fence value at which the copy into it is done. |
| `SlotTable` | It is the capture side's record of which textures the compositor still reads. `pick_released` picks only a texture the reader has released, without the `RECLAIM` time limit, as an export ring needs. |
| `Handoff` | It is the compositor side's frame waiting for its copy to finish. |
| `check_import(backend, capture, compositor)` | It returns the `Fallback` when the compositor's device cannot open the ring: a backend other than Direct3D 12 and Vulkan, another adapter, or a Vulkan device without `VK_KHR_external_memory_win32`. |
| `publish_import_caps(device)` (`wgpu` feature) | On Linux it records the formats and single-plane DRM format modifiers the compositor's Vulkan device can sample as DMA-BUFs, and the device's UUID; `dmabuf_caps` returns them to the capture side. A device that cannot import any, or a device on another backend, clears them. Elsewhere it does nothing. |
| `DmabufStream` (Linux) | It is the capture side's record of one screen-cast stream's DMA-BUF buffers. `set_format` takes the negotiated size, format and modifier, `buffers_changed` forgets the buffers when the stream adds or removes one, and `frame(fd, offset, stride)` duplicates a new buffer's descriptor into a free slot of a ring made with `SharedRing::dmabuf` (up to `MAX_BUFFERS`, 16) and returns a `SharedFrame`. A buffer whose offset or stride changes starts a new ring. |
| `check_dmabuf(vulkan, extensions, caps, device, layout)` | It returns the `Fallback` when a DMA-BUF ring cannot be opened: `UnsupportedBackend`, `MissingExtension`, `OtherAdapter`, or `Modifier` for a format and modifier the device cannot read. |
| `HeldBuffers` (Linux, `pipewire` feature) | It keeps a screen-cast stream's DMA-BUF buffers from the producer until the GPU has read their frames: `Dequeued::take` takes the stream's newest buffer, `keep(buffer, frame, limit)` keeps it, `release(stream, released)` gives back those whose frames have been read and those whose frames were dropped without a draw, and `forget` drops a buffer the stream removed. It keeps at most `MAX_HELD` (3) buffers and at most the limit the caller gives; a buffer beyond that goes back unread and its frame is not shown. `HoldQueue` is its bookkeeping, without PipeWire. |
| `SharedFrame::watched(ring, slot, ready)` | It makes a frame whose copies share a record of whether a draw read it. `watch` gives the capture side a `FrameWatch`, whose `dropped_undrawn` reports that every copy is gone and none was drawn; the importer calls `mark_drawn` when it submits a draw. `DmabufStream` makes its frames this way. |
| `SharedRing::release_dmabuf(ready)` (Linux) | The importer records that the GPU has finished every draw up to the frame `ready` of a DMA-BUF ring; `DmabufStream::released` reads it on the capture side, for all of the stream's rings. |
| `dmabuf_in_memory(len)` (Linux) | It makes a DMA-BUF over `len` bytes of sealed memory through `/dev/udmabuf`, for the bench and tests. |
| `Importer` (`wgpu` feature) | It opens a ring on the compositor's device, takes the waiting frame once its copy is done, draws it into the caller's texture and signals the released fence. `Importer::poll` hands back finished textures on a frame without a draw. |
| `Opaque` (`wgpu` feature) | It copies one texture into another of the same size with alpha set to 1 (`draw`), or with each texel's alpha kept and its premultiplied color made straight (`draw_keeping_alpha`). |
| `ExportRing::open(adapter, width, height)` | It makes `SLOTS` (3) shared NV12 textures and two shared fences on a Direct3D 11 device of that adapter, the device an encoder opens its session on (`device`, `texture`). Both sides must be even and not zero (`Fallback::ImportFailed`); elsewhere than Windows it returns `Fallback::Unsupported`. `ExportRing::detached` makes a ring without textures for tests. |
| `ExportRing::buffer(slot)`, `ExportRing::render_node()` (Linux) | They return the ring's `ExportBuffer` of a slot (the DMA-BUF descriptor, its size, its DRM format modifier, and the offset and row pitch of each plane) and the device number of the compositor GPU's DRM render node, for an encoder to import the buffers on that node. Each image is the ring's size rounded up to multiples of 16 (`ExportRing::padded_size`), with the picture at its top left, so an H.264 encoder reads its padded rows from the buffer; each copy fills the padding with the picture's last column and row. A ring made elsewhere has neither. |
| `nv12_planes_fit(width, height, size, offsets, pitches, linear)` | It says whether an NV12 picture's two planes lie inside a buffer of `size` bytes, with rows at least the picture's width in a linear layout. |
| `ExportFrame` | It names one texture of an export ring and the ready fence value at which the copy into it is done. |
| `Feed` | It is the encoder side's queue: `offer` adds a frame, `take_ready` returns the oldest frame once its copy is done, `out(packets)` hands back the textures of that many of the oldest frames being encoded (one packet per picture), `drain` forgets every frame and hands back the textures whose copies have finished, `copy_running` says whether the oldest waiting frame's copy is still running, and `stalled` says whether every texture is held while nothing is left to encode. |
| `check_export(backend, compositor)` | It returns the adapter to make the export ring on, or the `Fallback`: a backend other than Direct3D 12 and Vulkan, an adapter the driver does not report, or a Vulkan device without `VK_KHR_external_memory_win32` or timeline semaphores. |
| `check_dmabuf_export(backend, compositor, modifiers)` | It returns the adapter of a Linux export ring, or the `Fallback`: the reasons of `check_export` with the DMA-BUF extensions in place of `VK_KHR_external_memory_win32`, then `Modifier` when the device has no DRM format modifier in which it can export an NV12 image of the ring's size. |
| `Exporter` (`wgpu` feature) | It opens an export ring on the compositor's device. `reserve` makes one-texel textures of the planes' formats outside the frame loop, `prepare` records a one-texel copy from each plane into them so the planes end in the copy-source state, `copy` records the copy of a frame's luma and chroma planes (8-bit one- and two-channel) into a free texture as a command buffer of its own, and `submitted` sets the ready fence after that submission and returns the `ExportFrame`. |

## Synchronization

The capture side copies each new desktop picture into a free texture of the ring and signals the
ready fence with the frame's value. The compositor reads the ready fence's completed value and
draws the frame only once the value has been reached. It does not queue a wait on the GPU, so a
capture device that stops answering does not stall the compositor's queue. After the draw, the
released fence is set to the same value, and the capture side writes that texture again. On
Direct3D 12 the compositor's queue signals the fence. On Vulkan the importer opens the two fences
on a Direct3D 12 device of the same adapter and reads the ready fence from the CPU. After the
draw's submission, an empty submission signals a timeline semaphore of the importer's own with the
frame's value, and the importer copies the semaphore's value to the released fence from the CPU on
its next `draw` or `poll`, without waiting. A frame that is not drawn is handed back the same way.
When an import is closed while its last draw still runs, the released fence gets that draw's
value and the semaphore is destroyed once the queue has finished the work, and closing the import
does not wait. Each Vulkan draw submits a barrier that takes the texture from the external
queue family before it reads the texture, and one that hands it back after. Each barrier is a
command buffer of its own, because wgpu does not let one encoder record both its own commands and
raw Vulkan commands. A texture the compositor has not started to read after `RECLAIM` (500 ms)
is taken back and written again, and the compositor then skips that frame. A texture whose draw
the compositor has submitted is written again only after the released fence passes its value. When
the compositor gets a newer frame while an older one's copy has finished and the newer one's has
not, it draws the older one first. When every texture is still being read, the capture keeps the
acquired desktop picture and copies
it on a later grab.

A DMA-BUF ring uses the kernel's implicit synchronization instead of fences. The capture side
duplicates each buffer's descriptor and keeps the buffer from the stream. The compositor polls the
duplicate for reading with a timeout of 0 and draws the frame only once the poll reports that no
write is pending, so a late buffer stays waiting while the compositor draws the rest of the scene.
When the GPU has finished the draw, and every draw submitted before it, the importer records the
frame's ready value, and the capture side gives that buffer and every earlier one back to the
stream the next time the stream hands it a buffer. A frame the compositor skips is recorded the
same way once the draws submitted before it have finished. A buffer whose frame was replaced or
dropped before any draw read it goes back once the last copy of the frame is gone. Each draw submits a barrier that takes the image from the external queue family in the
`GENERAL` layout to `SHADER_READ_ONLY_OPTIMAL`, and one that hands it back after.

An export ring runs the same way in the other direction. The compositor copies a frame into a free
texture and sets the ready fence after the copy: on Direct3D 12 the queue signals it, and on
Vulkan a timeline semaphore signaled after the copy is copied to the fence from the CPU on the
exporter's next `copy` or `poll`. The encoder side reads the ready fence's completed value and
encodes a frame only once the value has been reached; a frame whose copy is late stays in the
`Feed`. Once the frame's packet is out, the encoder's Direct3D 11 context signals the released
fence with the frame's value, and the compositor writes that texture again. When an encoder stops,
only the textures whose copies have finished are handed back. Neither side
queues a wait on the GPU. wgpu cannot write NV12 textures, so the export ring's textures stay
wgpu-hal textures and the copies are raw commands in a command buffer of their own. On Vulkan that
command buffer takes the image from the external queue family before the copies and hands it back
in the `GENERAL` layout after them. A texture is written again only once the encoder has released
it, however long that takes, and when every texture is still being encoded the frame is not
exported. One encoder session reads each export ring.

On Linux the export ring has no fences. After each copy an empty submission signals a timeline
semaphore of the exporter's own with the frame's value, and `ExportRing::ready` reads the
semaphore's value from the CPU without waiting, from either side, for as long as the exporter
exists; after that the ring keeps the last value. VA-API takes no fence with a picture, so the
encoder side encodes a frame only once that value has been reached. The images have two memory
planes in a DRM format modifier the driver lists for NV12 with transfer writes and exportable
memory, and dedicated memory of their own. The copies take each image from the external queue
family and hand it back in the `GENERAL` layout, as on Windows. When the exporter is dropped while
a copy still runs, its images and the semaphore are destroyed once the queue has finished the
work, without waiting.

## Limits and failures

- `Duplication::open` returns `Fallback::Rotated` for a rotated output, `Fallback::Format` when
  desktop duplication does not give 8-bit BGRA pictures, and `Fallback::Unsupported` when
  the Direct3D 11 device has no shared fences (`ID3D11Device5`) or the system does not allow
  desktop duplication.
- The importer records `Fallback::UnsupportedBackend`, `Fallback::OtherAdapter`,
  `Fallback::MissingExtension` or `Fallback::ImportFailed` on the ring. The capture side then
  returns that reason from `grab`, and the caller captures through CPU memory from then on.
- On Vulkan the textures open only on a device created with wgpu's
  `Features::VULKAN_EXTERNAL_MEMORY_WIN32` that has timeline semaphores (Vulkan 1.2), the adapter
  must report its LUID, and the system must be able to create a Direct3D 12 device on that adapter
  for the fences.
- On Vulkan the capture side owns each imported image in the `GENERAL` layout. The barrier at the
  start of a draw moves it to `SHADER_READ_ONLY_OPTIMAL`, and the barrier at the end moves it back.
- `Exporter::open` returns `Fallback::UnsupportedBackend`, `Fallback::OtherAdapter` (the driver
  does not report its adapter), `Fallback::MissingExtension`, `Fallback::Unsupported` or
  `Fallback::ImportFailed`. An encoder that cannot open a session on the export ring's device
  records `Fallback::NoEncoder` on the ring, and `Exporter::copy` then returns it. The caller reads
  the frames back through CPU memory instead. `ExportRing::release` also records
  `Fallback::NoEncoder` when the released fence's signal cannot be queued.
- On Linux `Exporter::open` returns `Fallback::UnsupportedBackend` for a device not on Vulkan,
  `Fallback::MissingExtension` for a device created without wgpu's
  `Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF` or without timeline semaphores, `Fallback::Modifier`
  when no format modifier fits, and `Fallback::ImportFailed` when an image, its memory or its
  descriptor cannot be made. A VA-API encoder that cannot import a buffer records
  `Fallback::ImportFailed` on the ring, and one that cannot open on the ring's render node records
  `Fallback::NoEncoder`. When the compositor's device stops before the last copy into a frame is
  seen to finish, the ring records `Fallback::DeviceLost`, and the frames after the last finished
  copy never become ready.
- The desktop's alpha channel is not reliable, so `Importer` draws a display's picture with
  alpha 1. A window's ring has `SharedRing::keeps_alpha` set: its picture keeps its alpha, and
  its color, which the system delivers multiplied by alpha, is divided by alpha.
- Pointer-only desktop updates are not passed on as new pictures.
- `WindowCapture` needs `GraphicsCaptureSession::IsSupported` and a free-threaded frame pool
  (`Direct3D11CaptureFramePool::CreateFreeThreaded`); a system without them gives
  `Fallback::Unsupported`. The pointer is not drawn into the window's picture, and the border the
  system can draw around a captured window is turned off where the system allows it. A picture
  taken while the window's size differs from the frame pool's buffers is dropped, and a window
  wider or taller than 16,384 pixels gives `CaptureError::Failed`.
- The window capture's Direct3D 11 device runs with multithread protection on, since the system
  writes the frame pool's buffers from its own thread.
- A DMA-BUF ring opens only on a Vulkan 1.1 device created with wgpu's
  `Features::VULKAN_EXTERNAL_MEMORY_DMA_BUF`, and only for single-plane BGRA, BGRX, RGBA and RGBX
  buffers. Before a buffer reaches the driver, the importer checks that its descriptor is a
  DMA-BUF and that its stride and offset hold the picture; a buffer that fails the check, or that
  the driver refuses, gives `Fallback::ImportFailed`.
- The capture side keeps at most `MAX_HELD` (3) buffers, and at most one fewer than the stream
  has, so the producer always has a buffer to write into. A buffer that arrives while that many
  are kept goes back unread, and its frame is not shown. A stream with fewer than two buffers is
  copied through memory.

## License

Licensed under the MIT license or the Apache License, Version 2.0, at your option.
