//! DXGI desktop duplication on a Direct3D 11 device, and the shared textures and fences it copies
//! into. Every system call of the capture side is here, and so is the encoder side of an export
//! ring: NV12 textures and two fences on a Direct3D 11 device of the compositor's adapter.

use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HANDLE, HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_FENCE_FLAG_SHARED, D3D11_RESOURCE_MISC_SHARED,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device,
    ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
    DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter1, IDXGIFactory1, IDXGIFactory4, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, IDXGIResource1,
};
use windows::core::{Interface, PCWSTR};

use crate::CaptureError;
use crate::fallback::Fallback;
use crate::ring::{AdapterId, SLOTS};

/// The NT handles of a ring's textures and fences. They stay open for the ring's life, so a
/// device can open them at any time; they are closed when the ring is dropped.
pub(crate) struct Handles {
    pub(crate) textures: [HANDLE; SLOTS],
    pub(crate) ready: HANDLE,
    pub(crate) released: HANDLE,
}

// SAFETY: the handles are process-wide kernel object handles; any thread may pass them to
// `OpenSharedHandle` or `CloseHandle`, and `Handles` only reads them after construction.
unsafe impl Send for Handles {}
// SAFETY: as above; shared references only read the handle values.
unsafe impl Sync for Handles {}

impl Handles {
    pub(crate) fn none() -> Handles {
        Handles { textures: [HANDLE::default(); SLOTS], ready: HANDLE::default(), released: HANDLE::default() }
    }

    /// Whether every handle is open.
    #[cfg_attr(not(feature = "wgpu"), allow(dead_code))]
    pub(crate) fn complete(&self) -> bool {
        self.textures.iter().chain([&self.ready, &self.released]).all(|h| !h.is_invalid())
    }
}

impl Drop for Handles {
    fn drop(&mut self) {
        for h in self.textures.iter().chain([&self.ready, &self.released]) {
            if !h.is_invalid() {
                // SAFETY: each valid handle was returned by `CreateSharedHandle` for this ring and
                // is closed exactly once, here.
                let _ = unsafe { CloseHandle(*h) };
            }
        }
    }
}

fn failed(what: &str, e: windows::core::Error) -> CaptureError {
    CaptureError::Failed(format!("{what}: {e}"))
}

/// A display being duplicated, with the ring's textures and fences on its device.
pub(crate) struct Output {
    _device: ID3D11Device,
    context: ID3D11DeviceContext4,
    duplication: IDXGIOutputDuplication,
    textures: [ID3D11Texture2D; SLOTS],
    ready: ID3D11Fence,
    released: ID3D11Fence,
    /// A desktop picture acquired and not copied yet, because every texture was in use.
    held: Option<ID3D11Texture2D>,
    /// No picture has been acquired yet; the first one is taken even when it reports no present.
    first: bool,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) adapter: AdapterId,
}

// SAFETY: the Direct3D 11 device is free-threaded, and its immediate context, the duplication and
// the textures are only used through `&mut Output`, so by one thread at a time.
unsafe impl Send for Output {}

/// The adapter and output whose output device name is `name`.
fn find(name: &str) -> Result<(IDXGIAdapter1, IDXGIOutput, windows::Win32::Graphics::Dxgi::DXGI_OUTPUT_DESC), CaptureError> {
    // SAFETY: creating a DXGI factory has no preconditions.
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| failed("listing the graphics adapters", e))?;
    for a in 0..64u32 {
        // SAFETY: `factory` is a valid factory; an index past the last adapter returns an error.
        let adapter = match unsafe { factory.EnumAdapters1(a) } {
            Ok(adapter) => adapter,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(failed("listing the graphics adapters", e)),
        };
        for o in 0..64u32 {
            // SAFETY: `adapter` is valid; an index past the last output returns an error.
            let output = match unsafe { adapter.EnumOutputs(o) } {
                Ok(output) => output,
                Err(_) => break,
            };
            // SAFETY: `output` is valid; `GetDesc` fills a structure it returns by value.
            let Ok(desc) = (unsafe { output.GetDesc() }) else { continue };
            let len = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(desc.DeviceName.len());
            let found = desc.DeviceName.get(..len).map(String::from_utf16_lossy);
            if found.as_deref() == Some(name) {
                return Ok((adapter, output, desc));
            }
        }
    }
    Err(CaptureError::Failed(format!("no display named {name}")))
}

/// Open desktop duplication of the display named `name`, with a ring of textures of its size.
pub(crate) fn open(name: &str) -> Result<(Output, Handles), CaptureError> {
    let (adapter, output, desc) = find(name)?;
    if desc.Rotation != DXGI_MODE_ROTATION_IDENTITY && desc.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED {
        return Err(CaptureError::Fallback(Fallback::Rotated));
    }
    // SAFETY: `adapter` is valid; `GetDesc1` returns its description by value.
    let luid = unsafe { adapter.GetDesc1() }.map_err(|e| failed("reading the graphics adapter", e))?.AdapterLuid;
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: the out pointers point at the two locals above, which outlive the call; a driver
    // type of UNKNOWN is required when an adapter is given.
    unsafe {
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|e| failed("creating a Direct3D 11 device", e))?;
    let (Some(device), Some(context)) = (device, context) else {
        return Err(CaptureError::Failed("creating a Direct3D 11 device gave no device".into()));
    };
    let unsupported = |_| CaptureError::Fallback(Fallback::Unsupported);
    let device5: ID3D11Device5 = device.cast().map_err(unsupported)?;
    let context: ID3D11DeviceContext4 = context.cast().map_err(unsupported)?;
    let output1: IDXGIOutput1 = output.cast().map_err(unsupported)?;
    // SAFETY: `output1` and `device` are valid, and `device` is on the adapter that owns the output.
    let duplication = unsafe { output1.DuplicateOutput(&device) }.map_err(|e| {
        if e.code() == DXGI_ERROR_UNSUPPORTED { CaptureError::Fallback(Fallback::Unsupported) } else { failed("starting desktop duplication", e) }
    })?;
    // SAFETY: `duplication` is valid; `GetDesc` returns its description by value.
    let dd = unsafe { duplication.GetDesc() };
    if dd.ModeDesc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
        return Err(CaptureError::Fallback(Fallback::Format));
    }
    if dd.Rotation != DXGI_MODE_ROTATION_IDENTITY && dd.Rotation != DXGI_MODE_ROTATION_UNSPECIFIED {
        return Err(CaptureError::Fallback(Fallback::Rotated));
    }
    let (width, height) = (dd.ModeDesc.Width, dd.ModeDesc.Height);
    if width == 0 || height == 0 {
        return Err(CaptureError::Lost("the display reports no size".into()));
    }
    let tex_desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED.0) as u32,
    };
    let mut handles = Handles::none();
    let mut made: Vec<ID3D11Texture2D> = Vec::with_capacity(SLOTS);
    for slot in 0..SLOTS {
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: `tex_desc` lives for the call and `tex` receives the texture.
        unsafe { device.CreateTexture2D(&tex_desc, None, Some(&mut tex)) }.map_err(|e| failed("creating a shared texture", e))?;
        let tex = tex.ok_or_else(|| CaptureError::Failed("creating a shared texture gave none".into()))?;
        let resource: IDXGIResource1 = tex.cast().map_err(unsupported)?;
        // SAFETY: the texture was created with SHARED_NTHANDLE; no name and default security.
        let handle = unsafe { resource.CreateSharedHandle(None, DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0, PCWSTR::null()) }
            .map_err(|e| failed("sharing a texture", e))?;
        if let Some(h) = handles.textures.get_mut(slot) {
            *h = handle;
        }
        made.push(tex);
    }
    let fence = || -> Result<(ID3D11Fence, HANDLE), CaptureError> {
        let mut f: Option<ID3D11Fence> = None;
        // SAFETY: `f` receives the fence; a shared fence needs Windows 10 version 1703, and an
        // older system returns an error.
        unsafe { device5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut f) }.map_err(unsupported)?;
        let f = f.ok_or(CaptureError::Fallback(Fallback::Unsupported))?;
        // SAFETY: the fence was created shared; no name and default security.
        let h = unsafe { f.CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null()) }.map_err(|e| failed("sharing a fence", e))?;
        Ok((f, h))
    };
    let (ready, ready_handle) = fence()?;
    handles.ready = ready_handle;
    let (released, released_handle) = fence()?;
    handles.released = released_handle;
    let textures: [ID3D11Texture2D; SLOTS] = made.try_into().map_err(|_| CaptureError::Failed("the shared textures could not be created".into()))?;
    let output = Output {
        _device: device,
        context,
        duplication,
        textures,
        ready,
        released,
        held: None,
        first: true,
        width,
        height,
        adapter: AdapterId::from_parts(luid.LowPart, luid.HighPart),
    };
    Ok((output, handles))
}

impl Output {
    /// The released fence's value: the newest frame the compositor has finished reading.
    pub(crate) fn released(&self) -> u64 {
        // SAFETY: `released` is a valid fence; reading its completed value does not wait.
        unsafe { self.released.GetCompletedValue() }
    }

    /// Acquire a new desktop picture without waiting, unless one is held already. Returns
    /// whether a picture is held.
    pub(crate) fn acquire(&mut self) -> Result<bool, CaptureError> {
        if self.held.is_some() {
            return Ok(true);
        }
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: `info` and `resource` outlive the call; a timeout of 0 does not wait.
        match unsafe { self.duplication.AcquireNextFrame(0, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Err(CaptureError::Lost(e.to_string())),
            Err(e) => return Err(failed("acquiring the desktop picture", e)),
        }
        // Only the pointer moved: no new picture.
        if info.LastPresentTime == 0 && !self.first {
            self.release_frame();
            return Ok(false);
        }
        let texture = resource.and_then(|r| r.cast::<ID3D11Texture2D>().ok());
        let Some(texture) = texture else {
            self.release_frame();
            return Err(CaptureError::Failed("the desktop picture is not a texture".into()));
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: `texture` is valid and `desc` outlives the call.
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            self.release_frame();
            return Err(CaptureError::Fallback(Fallback::Format));
        }
        if (desc.Width, desc.Height) != (self.width, self.height) {
            self.release_frame();
            return Err(CaptureError::Lost("the display changed size".into()));
        }
        self.first = false;
        self.held = Some(texture);
        Ok(true)
    }

    fn release_frame(&mut self) {
        // SAFETY: called only after a successful `AcquireNextFrame`.
        let _ = unsafe { self.duplication.ReleaseFrame() };
    }

    /// Copy the held picture into texture `slot` on the GPU, signal `ready` on the ready fence
    /// after the copy, and give the picture back to the system.
    pub(crate) fn copy_held(&mut self, slot: usize, ready: u64) -> Result<(), CaptureError> {
        let Some(src) = self.held.take() else { return Err(CaptureError::Failed("no desktop picture is held".into())) };
        let Some(dst) = self.textures.get(slot) else {
            self.release_frame();
            return Err(CaptureError::Failed("no such texture in the ring".into()));
        };
        // SAFETY: both textures are on this device, have the same size and format, and the
        // destination is not being read: the caller picked it from the free slots.
        unsafe { self.context.CopyResource(dst, &src) };
        // SAFETY: `ready` is a fence of this device; the signal runs after the copy on the GPU.
        let signaled = unsafe { self.context.Signal(&self.ready, ready) };
        // SAFETY: the context is valid; flushing sends the copy and the signal to the GPU.
        unsafe { self.context.Flush() };
        drop(src);
        self.release_frame();
        signaled.map_err(|e| failed("signaling the copied picture", e))
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        if self.held.take().is_some() {
            self.release_frame();
        }
    }
}

/// The encoder side of an export ring: NV12 textures shared through NT handles, the ready fence the
/// compositor sets once a texture holds a frame, and the released fence the encoder sets once it
/// has read one.
pub(crate) struct Nv12Ring {
    device: ID3D11Device,
    /// Only [`Nv12Ring::release`] uses it, from the encoder's thread; the lock keeps a second
    /// thread off the immediate context.
    context: std::sync::Mutex<ID3D11DeviceContext4>,
    textures: [ID3D11Texture2D; SLOTS],
    ready: ID3D11Fence,
    released: ID3D11Fence,
    pub(crate) handles: Handles,
}

// SAFETY: the Direct3D 11 device and its fences are free-threaded, the textures are only handed out
// as new references, and the immediate context is used only under its lock.
unsafe impl Send for Nv12Ring {}
// SAFETY: as above; shared references read the fences and clone the COM pointers.
unsafe impl Sync for Nv12Ring {}

/// An NV12 ring of `width × height` on the adapter `adapter`. Both sides must be even and not zero.
pub(crate) fn nv12_ring(adapter: AdapterId, width: u32, height: u32) -> Result<Nv12Ring, Fallback> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(Fallback::ImportFailed);
    }
    let (low, high) = adapter.parts();
    // SAFETY: creating a DXGI factory has no precondition.
    let factory: IDXGIFactory4 = unsafe { CreateDXGIFactory1() }.map_err(|_| Fallback::Unsupported)?;
    // SAFETY: `factory` is a valid factory; looking up an adapter has no other effect.
    let dxgi: IDXGIAdapter1 = unsafe { factory.EnumAdapterByLuid(LUID { LowPart: low, HighPart: high }) }.map_err(|_| Fallback::OtherAdapter)?;
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: the out pointers point at the two locals above, which outlive the call; a driver
    // type of UNKNOWN is required when an adapter is given.
    unsafe {
        D3D11CreateDevice(
            &dxgi,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            Default::default(),
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|_| Fallback::ImportFailed)?;
    let (Some(device), Some(context)) = (device, context) else { return Err(Fallback::ImportFailed) };
    let device5: ID3D11Device5 = device.cast().map_err(|_| Fallback::Unsupported)?;
    let context: ID3D11DeviceContext4 = context.cast().map_err(|_| Fallback::Unsupported)?;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0 | D3D11_RESOURCE_MISC_SHARED.0) as u32,
    };
    let mut handles = Handles::none();
    let mut made: Vec<ID3D11Texture2D> = Vec::with_capacity(SLOTS);
    for slot in 0..SLOTS {
        let mut tex: Option<ID3D11Texture2D> = None;
        // SAFETY: `desc` lives for the call and `tex` receives the texture.
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut tex)) }.map_err(|_| Fallback::Unsupported)?;
        let tex = tex.ok_or(Fallback::Unsupported)?;
        let resource: IDXGIResource1 = tex.cast().map_err(|_| Fallback::Unsupported)?;
        // SAFETY: the texture was created with SHARED_NTHANDLE; no name and default security.
        let handle = unsafe { resource.CreateSharedHandle(None, DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0, PCWSTR::null()) }
            .map_err(|_| Fallback::Unsupported)?;
        if let Some(h) = handles.textures.get_mut(slot) {
            *h = handle;
        }
        made.push(tex);
    }
    let fence = || -> Result<(ID3D11Fence, HANDLE), Fallback> {
        let mut f: Option<ID3D11Fence> = None;
        // SAFETY: `f` receives the fence; a shared fence needs Windows 10 version 1703, and an
        // older system returns an error.
        unsafe { device5.CreateFence(0, D3D11_FENCE_FLAG_SHARED, &mut f) }.map_err(|_| Fallback::Unsupported)?;
        let f = f.ok_or(Fallback::Unsupported)?;
        // SAFETY: the fence was created shared; no name and default security.
        let h = unsafe { f.CreateSharedHandle(None, GENERIC_ALL.0, PCWSTR::null()) }.map_err(|_| Fallback::Unsupported)?;
        Ok((f, h))
    };
    let (ready, ready_handle) = fence()?;
    handles.ready = ready_handle;
    let (released, released_handle) = fence()?;
    handles.released = released_handle;
    let textures: [ID3D11Texture2D; SLOTS] = made.try_into().map_err(|_| Fallback::Unsupported)?;
    Ok(Nv12Ring { device, context: std::sync::Mutex::new(context), textures, ready, released, handles })
}

impl Nv12Ring {
    pub(crate) fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub(crate) fn texture(&self, slot: usize) -> Option<&ID3D11Texture2D> {
        self.textures.get(slot)
    }

    /// The ready fence's value: the newest frame whose copy into its texture has finished.
    pub(crate) fn ready(&self) -> u64 {
        // SAFETY: `ready` is a valid fence; reading its completed value does not wait.
        unsafe { self.ready.GetCompletedValue() }
    }

    /// The released fence's value: the newest frame the encoder has finished reading.
    pub(crate) fn released(&self) -> u64 {
        // SAFETY: `released` is a valid fence; reading its completed value does not wait.
        unsafe { self.released.GetCompletedValue() }
    }

    /// Set the released fence to `value` once the work already queued on the device has run. The
    /// encoder calls it after the packet of the frame has come out, when the texture has been
    /// read. Returns whether the signal was queued.
    pub(crate) fn release(&self, value: u64) -> bool {
        let Ok(context) = self.context.lock() else { return false };
        // SAFETY: `released` is a fence of this device; the signal runs on the GPU after the work
        // queued so far, without waiting here.
        let signaled = unsafe { context.Signal(&self.released, value) }.is_ok();
        // SAFETY: the context is valid; flushing sends the signal to the GPU.
        unsafe { context.Flush() };
        signaled
    }
}
