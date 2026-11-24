# craft-gpu-interop

craft-gpu-interop hands captured display pictures from the capture side to a wgpu compositor
without copying them through CPU memory. On Windows it captures a display with DXGI desktop
duplication into Direct3D 11 textures that are shared through NT handles, and opens those textures
and two shared fences on a wgpu Direct3D 12 or Vulkan device. Without the `wgpu` feature the crate
builds for every target, WebAssembly included, and holds the types both sides exchange.

Only the private `dxgi_ffi` (capture side), `d3d12_ffi` and `vulkan_ffi` (compositor side) modules
contain `unsafe` code. They call Direct3D through the `windows` crate's bindings, Vulkan through
`ash`, and wgpu through `wgpu::hal`.

## Contents

| Item | Description |
|---|---|
| `Duplication::open(device_name)` | It opens desktop duplication of the output with that GDI device name (`\\.\DISPLAY1`) and a ring of `SLOTS` (3) shared BGRA textures. Elsewhere than Windows it returns `CaptureError::Fallback(Fallback::Unsupported)`. |
| `Duplication::grab()` | It returns at once: `Grab::Frame` with a new picture, or `Grab::Unchanged`. `CaptureError::Lost` means the desktop changed mode or was switched and the duplication must be opened again; `CaptureError::Fallback` means the pictures must go through CPU memory. |
| `SharedRing` | It holds one capture's shared handles, its size, the adapter that owns the textures, and the reason the compositor gave for refusing them (`fallback`, `refuse`). `SharedRing::detached` makes a ring without textures for tests. |
| `SharedFrame` | It names one texture of a ring and the ready fence value at which the copy into it is done. |
| `SlotTable` | It is the capture side's record of which textures the compositor still reads. |
| `Handoff` | It is the compositor side's frame waiting for its copy to finish. |
| `check_import(backend, capture, compositor)` | It returns the `Fallback` when the compositor's device cannot open the ring: a backend other than Direct3D 12 and Vulkan, another adapter, or a Vulkan device without `VK_KHR_external_memory_win32`. |
| `Importer` (`wgpu` feature) | It opens a ring on the compositor's device, takes the waiting frame once its copy is done, draws it into the caller's texture and signals the released fence. `Importer::poll` hands back finished textures on a frame without a draw. |
| `Opaque` (`wgpu` feature) | It copies one texture into another of the same size with alpha set to 1. |

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
does not wait. Each Vulkan draw records a barrier that takes the texture from the external
queue family before it reads the texture, and one that hands it back after. A texture the
compositor never releases is written again after `RECLAIM` (500 ms).
When every texture is still being read, the capture keeps the acquired desktop picture and copies
it on a later grab.

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
- The desktop's alpha channel is not reliable, so `Importer` draws the picture with alpha 1.
- Pointer-only desktop updates are not passed on as new pictures.
- Window capture is not covered.

## License

Licensed under the MIT license or the Apache License, Version 2.0, at your option.
