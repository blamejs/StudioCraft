//! Windows.Graphics.Capture of one window on a Direct3D 11 device, and the copy of its pictures
//! into a ring of shared textures on that device. Every system call of the window capture is here.

use windows::Graphics::Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D11::{D3D11_TEXTURE2D_DESC, ID3D11Device, ID3D11DeviceContext4, ID3D11Multithread, ID3D11Texture2D};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::WindowsAndMessaging::IsWindow;
use windows::core::Interface;

use crate::CaptureError;
use crate::dxgi_ffi::{self, BgraRing, Handles};
use crate::fallback::Fallback;
use crate::ring::AdapterId;
use crate::window_capture::{Step, open_error, sides};

/// Buffers in the system's frame pool: one the system writes while the capture reads the other.
const BUFFERS: i32 = 2;

/// One captured picture, held until it is copied or replaced. Dropping it gives its buffer back
/// to the frame pool.
pub(crate) struct Picture {
    frame: Direct3D11CaptureFrame,
    texture: ID3D11Texture2D,
    /// The window's size when the picture was taken.
    pub(crate) content: (i32, i32),
    /// The size of the buffer the picture is in.
    pub(crate) size: (u32, u32),
}

impl Drop for Picture {
    fn drop(&mut self) {
        let _ = self.frame.Close();
    }
}

/// A window being captured, and the ring of shared textures its pictures are copied into.
pub(crate) struct Session {
    /// The window's handle, as an integer; [`hwnd`] makes the handle.
    window: isize,
    device: ID3D11Device,
    context: ID3D11DeviceContext4,
    winrt_device: IDirect3DDevice,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    _item: GraphicsCaptureItem,
    /// The size the frame pool's buffers were made at.
    pub(crate) pool_size: (i32, i32),
    pub(crate) adapter: AdapterId,
    ring: Option<BgraRing>,
}

// SAFETY: the Direct3D 11 device is free-threaded and runs with multithread protection on, so the
// system's capture thread and this one may both use its immediate context. The frame pool was
// made free-threaded, and the session, the item and the textures are only used through
// `&mut Session`, so by one thread at a time.
unsafe impl Send for Session {}

fn hwnd(window: isize) -> HWND {
    HWND(window as *mut core::ffi::c_void)
}

fn code(e: &windows::core::Error) -> i32 {
    e.code().0
}

/// A captured picture's texture, the window's size when it was taken, and the texture's size.
fn read(frame: &Direct3D11CaptureFrame) -> Result<(ID3D11Texture2D, (i32, i32), (u32, u32)), CaptureError> {
    let content = frame.ContentSize().map_err(|e| CaptureError::Lost(format!("the captured picture has no size: {e}")))?;
    let surface = frame.Surface().map_err(|e| CaptureError::Lost(format!("the captured picture has no surface: {e}")))?;
    let access: IDirect3DDxgiInterfaceAccess = surface.cast().map_err(|e| CaptureError::Failed(format!("reading the captured picture: {e}")))?;
    // SAFETY: `access` belongs to a surface of this device; it hands out a new reference.
    let texture: windows::core::Result<ID3D11Texture2D> = unsafe { access.GetInterface() };
    let texture = texture.map_err(|e| CaptureError::Failed(format!("reading the captured picture: {e}")))?;
    let mut desc = D3D11_TEXTURE2D_DESC::default();
    // SAFETY: `texture` is valid and `desc` outlives the call.
    unsafe { texture.GetDesc(&mut desc) };
    if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
        return Err(CaptureError::Fallback(Fallback::Format));
    }
    Ok((texture, (content.Width, content.Height), (desc.Width, desc.Height)))
}

impl Session {
    /// Capture the window `window` on a Direct3D 11 device of the adapter that drives the display
    /// named `display`, the display the window is on.
    pub(crate) fn open(window: isize, display: &str) -> Result<Session, CaptureError> {
        match GraphicsCaptureSession::IsSupported() {
            Ok(true) => {}
            Ok(false) => return Err(CaptureError::Fallback(Fallback::Unsupported)),
            Err(e) => return Err(open_error(Step::Support, code(&e), &e.message())),
        }
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
            .map_err(|e| open_error(Step::Support, code(&e), &e.message()))?;
        // SAFETY: `interop` is valid; a handle that names no window returns an error.
        let item: windows::core::Result<GraphicsCaptureItem> = unsafe { interop.CreateForWindow(hwnd(window)) };
        let item = item.map_err(|e| open_error(Step::Item, code(&e), &e.message()))?;
        let adapter = dxgi_ffi::adapter_of(display)?;
        let (device, context, adapter) = dxgi_ffi::bgra_device(&adapter)?;
        // The system's capture thread uses the device while this one copies on its immediate
        // context, so the context's calls are serialized.
        let mt: ID3D11Multithread = context.cast().map_err(|e| open_error(Step::Device, code(&e), &e.message()))?;
        // SAFETY: `mt` is the immediate context's multithread interface; turning protection on has
        // no other precondition. The previous setting it returns is not needed.
        let _ = unsafe { mt.SetMultithreadProtected(true) };
        let dxgi: IDXGIDevice = device.cast().map_err(|e| open_error(Step::Device, code(&e), &e.message()))?;
        // SAFETY: `dxgi` is the DXGI side of a valid Direct3D 11 device.
        let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }.map_err(|e| open_error(Step::Device, code(&e), &e.message()))?;
        let winrt_device: IDirect3DDevice = inspectable.cast().map_err(|e| open_error(Step::Device, code(&e), &e.message()))?;
        let size = item.Size().map_err(|e| open_error(Step::Item, code(&e), &e.message()))?;
        let pool_size = (size.Width.max(1), size.Height.max(1));
        // A window larger than a shared texture is refused before the pool's buffers are made.
        sides(pool_size.0, pool_size.1)?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &winrt_device,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            BUFFERS,
            SizeInt32 { Width: pool_size.0, Height: pool_size.1 },
        )
        .map_err(|e| open_error(Step::Pool, code(&e), &e.message()))?;
        let session = pool.CreateCaptureSession(&item).map_err(|e| open_error(Step::Session, code(&e), &e.message()))?;
        // The pointer is not drawn into the window's picture, and the yellow border some systems
        // draw around a captured window is turned off where the system allows it. Older systems
        // do not have these settings and keep their defaults.
        let _ = session.SetIsCursorCaptureEnabled(false);
        let _ = session.SetIsBorderRequired(false);
        session.StartCapture().map_err(|e| open_error(Step::Session, code(&e), &e.message()))?;
        Ok(Session { window, device, context, winrt_device, pool, session, _item: item, pool_size, adapter, ring: None })
    }

    /// Whether the window still exists.
    pub(crate) fn alive(&self) -> bool {
        // SAFETY: `IsWindow` accepts any handle value and only reports whether it names a window.
        unsafe { IsWindow(Some(hwnd(self.window))) }.as_bool()
    }

    /// The newest picture the system has delivered, without waiting; older ones are given back.
    pub(crate) fn next(&mut self) -> Result<Option<Picture>, CaptureError> {
        let mut newest = None;
        // The pool holds at most `BUFFERS` pictures, so this ends after a few calls.
        for _ in 0..=BUFFERS {
            match self.pool.TryGetNextFrame() {
                // An older picture goes back to the pool at once, so the system can write the next.
                Ok(frame) => {
                    if let Some(older) = newest.replace(frame) {
                        let _ = older.Close();
                    }
                }
                // No picture: the call gives an empty result, not an error code.
                Err(e) if e.code().is_ok() => break,
                Err(e) => return Err(CaptureError::Lost(format!("the window capture stopped: {e}"))),
            }
        }
        let Some(frame) = newest else { return Ok(None) };
        match read(&frame) {
            Ok((texture, content, size)) => Ok(Some(Picture { frame, texture, content, size })),
            Err(e) => {
                let _ = frame.Close();
                Err(e)
            }
        }
    }

    /// Make the frame pool's buffers `width × height`, so the next pictures hold the whole window.
    pub(crate) fn resize_pool(&mut self, width: i32, height: i32) -> Result<(), CaptureError> {
        self.pool
            .Recreate(&self.winrt_device, DirectXPixelFormat::B8G8R8A8UIntNormalized, BUFFERS, SizeInt32 { Width: width, Height: height })
            .map_err(|e| CaptureError::Lost(format!("the window capture could not follow the window's size: {e}")))?;
        self.pool_size = (width, height);
        Ok(())
    }

    /// Replace the ring with one of `width × height`, and return its shared handles.
    pub(crate) fn new_ring(&mut self, width: u32, height: u32) -> Result<Handles, CaptureError> {
        self.ring = None;
        let (ring, handles) = dxgi_ffi::bgra_ring(&self.device, width, height)?;
        self.ring = Some(ring);
        Ok(handles)
    }

    /// The released fence's value: the newest picture the compositor has finished reading.
    pub(crate) fn released(&self) -> u64 {
        let Some(ring) = &self.ring else { return 0 };
        // SAFETY: `released` is a valid fence; reading its completed value does not wait.
        unsafe { ring.released.GetCompletedValue() }
    }

    /// Copy `picture` into texture `slot` on the GPU, signal `ready` on the ready fence after the
    /// copy, and give the picture's buffer back to the frame pool. The picture has the ring's size.
    pub(crate) fn copy(&mut self, picture: Picture, slot: usize, ready: u64) -> Result<(), CaptureError> {
        let Some(ring) = &self.ring else { return Err(CaptureError::Failed("the window capture has no ring".into())) };
        let Some(dst) = ring.textures.get(slot) else { return Err(CaptureError::Failed("no such texture in the ring".into())) };
        // SAFETY: both textures are on this device and have the same size and format, and the
        // destination is not being read: the caller picked it from the free slots.
        unsafe { self.context.CopyResource(dst, &picture.texture) };
        // SAFETY: `ready` is a fence of this device; the signal runs after the copy on the GPU.
        let signaled = unsafe { self.context.Signal(&ring.ready, ready) };
        // SAFETY: the context is valid; flushing sends the copy and the signal to the GPU.
        unsafe { self.context.Flush() };
        drop(picture);
        signaled.map_err(|e| CaptureError::Failed(format!("signaling the copied picture: {e}")))
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}
