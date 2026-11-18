# craft-gpu-device

Choosing and opening a wgpu device for a Craft app: an adapter choice that the calling application
sets, fixes for drivers that crash, a software fallback, a capability report, and device health
with recovery after a device loss. The crate depends on `wgpu` 30, `pollster` and `log`, with
`serde` as an optional feature (on by default), and depends on no app crate.

## Contents

| Item | What |
|---|---|
| `Candidate` | One listed adapter as plain data: name, backend, device type, PCI vendor, device and bus, driver, feature count, and whether it can draw to the window's surface. |
| `list(instance, surface)` | Every adapter the instance lists, as `wgpu::Adapter`s and as `Candidate`s. |
| `native_instance`, `native_descriptor` | A wgpu instance over Vulkan, Metal and DirectX 12, plus OpenGL except on Windows. wgpu's `WGPU_*` flag variables apply. |
| `choose(candidates, power)` | wgpu's own adapter rule with two corrections: the newest version of a driver that lists one GPU twice, and DirectX 12 instead of Vulkan on Intel Gen 9 to Gen 11 integrated GPUs (`intel_gen9_to_gen11`). |
| `Preference` | `Auto` (high performance), `LowPower`, `Adapter(AdapterKey)` or `Software`. The serde form is `{"kind":"auto"}` or `{"kind":"adapter","name":…,"backend":…,"vendor":…,"device":…,"pciBusId":…,"driver":…}`. |
| `AdapterKey` | What identifies an adapter across runs: backend, PCI vendor, device and bus, the Vulkan driver's name, and the name when the backend reports no PCI ids. |
| `select(candidates, preference)` | The adapter to draw with, as a `Selection` with its `Notice`s. |
| `ranked(candidates, preference)` | The order in which to try opening a device: the selection, then the other usable adapters by type with software after hardware. It never includes an older version of a driver listed twice, or the Vulkan entry of an Intel Gen 9 to Gen 11 GPU on Intel's Windows driver. |
| `open(instance, surface, preference, request)` | It is available on native targets only, like `list`. It opens a device through `ranked`, then wgpu's fallback adapter, and returns `Opened` (adapter, device, queue, `Capabilities`, `DeviceHealth`, notices) or `OpenError`. |
| `pick(adapters, surface, preference)` | `select` over listed adapters, for egui-wgpu's `native_adapter_selector`. |
| `Capabilities`, `Vendor` | NV12 and P010 (sampled only in wgpu 30), 16-bit norm, `f16` and timestamp support, the largest texture and buffer, the vendor, and whether the adapter is a software one. |
| `DeviceHealth`, `Fault`, `WaitOutcome` | `DeviceHealth` records a device loss or an uncaptured error instead of panicking. Its polls and bounded waits do not touch a lost device. |
| `Recovery`, `RecoveryPolicy` | The preference to open after a loss: the configured preference, and `Software` once the device was lost 3 times in 10 minutes. Both values can be set. |
| `Notice` | The adapter asked for is missing, no software adapter is listed, only a software adapter can draw, an adapter failed to open, or drawing moved to software after losses. The `Display` text is English, for the log. |

## Limits and failures

- With no usable adapter, `select` returns `None` and `open` returns `OpenError::NoAdapter`.
- An adapter that lacks `Request::required_features`, or whose `request_device` fails, adds a
  `Notice::DeviceFailed`, and `open` tries the next one. When every adapter fails, the error lists
  each failure.
- A named adapter that is no longer listed, or can no longer draw to the surface, gives the
  automatic choice and `Notice::AdapterMissing`.
- A driver that ends the process cannot be caught. `choose` and `ranked` never use an older version
  of a driver that lists one GPU twice, or the Vulkan entry of an Intel Gen 9 to Gen 11 GPU on
  Intel's Windows driver. When those are the only adapters that can draw and wgpu's fallback
  adapter does not open either, `open` returns `OpenError::UnsafeDriver` with their names.
- When a driver reports no bus id, a second GPU of the same model on the same driver looks like an
  older version. It is then used through its other backend (DirectX 12 on Windows, OpenGL
  elsewhere).
- Only an error whose cause is wgpu's "Parent device is lost" counts as a loss. The same words in a
  resource's label do not count. An error about a destroyed buffer or texture is recorded as an
  error.
- `wait_timeout` and `wait_timeout_cancelable` wait for one submission index. To wait for
  everything submitted so far, pass `queue.submit([])`. Work submitted later does not extend the
  wait.
- `DeviceHealth::watch` replaces the device's uncaptured-error handler. Polls catch the panic that
  `Device::poll` raises on a lost device and record the loss.

## Tests

The selection tests run on `Candidate` lists from real machines. The `open` tests use wgpu's noop
backend (the `noop` feature, enabled for tests only), so they do not need a GPU. One test compares
`choose` with wgpu's own `request_adapter` on the machine that runs it, and passes when no adapter
is listed.

## License

Licensed under either of MIT or Apache-2.0, at your option.
