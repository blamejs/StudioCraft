//! The device an encode session is opened on: a Direct3D 11 device of the first NVIDIA adapter (or
//! the caller's own device, for texture input) on Windows, the retained primary CUDA context of
//! device 0 on Linux. The handles stay in this module.

use std::ffi::c_void;

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub use cuda::Device;
#[cfg(target_os = "windows")]
pub use directx::Device;
#[cfg(all(test, target_os = "windows"))]
pub use directx::{test_device, test_texture};
#[cfg(not(any(target_os = "windows", all(target_os = "linux", target_pointer_width = "64"))))]
pub use none::Device;

#[cfg(not(any(target_os = "windows", all(target_os = "linux", target_pointer_width = "64"))))]
mod none {
    use super::c_void;

    /// No device: the encoder runs on Windows and 64-bit Linux only.
    pub struct Device;

    impl Device {
        pub const KIND: u32 = 0;

        pub fn new() -> Result<Self, String> {
            Err("NVENC runs on Windows and 64-bit Linux".into())
        }

        pub fn as_raw(&self) -> *mut c_void {
            std::ptr::null_mut()
        }
    }
}

#[cfg(target_os = "windows")]
mod directx {
    use super::c_void;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
    use windows::Win32::Graphics::Direct3D11::{D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION, D3D11CreateDevice, ID3D11Device};
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1};
    use windows::core::Interface;

    /// NVIDIA's PCI vendor id.
    const NVIDIA: u32 = 0x10DE;

    pub struct Device(ID3D11Device);

    impl Device {
        pub const KIND: u32 = super::super::abi::NV_ENC_DEVICE_TYPE_DIRECTX;

        pub fn new() -> Result<Self, String> {
            nvidia_device().map(Self)
        }

        /// The caller's device, so the session can read textures made on it.
        pub fn shared(device: ID3D11Device) -> Self {
            Self(device)
        }

        pub fn d3d11(&self) -> &ID3D11Device {
            &self.0
        }

        pub fn as_raw(&self) -> *mut c_void {
            self.0.as_raw()
        }
    }

    /// A new Direct3D 11 device on the first NVIDIA adapter, for tests.
    #[cfg(test)]
    pub fn test_device() -> Result<ID3D11Device, String> {
        nvidia_device()
    }

    /// A `width` × `height` NV12 (or P010 when `ten_bit`) texture on `device`, every sample set to
    /// mid gray, made the way an encoder input texture is: default usage, bound as a render target.
    #[cfg(test)]
    pub fn test_texture(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        ten_bit: bool,
    ) -> Result<windows::Win32::Graphics::Direct3D11::ID3D11Texture2D, String> {
        use windows::Win32::Graphics::Direct3D11::{D3D11_BIND_RENDER_TARGET, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT};
        use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_SAMPLE_DESC};
        let samples = (width as usize) * (height as usize) * 3 / 2;
        let (format, data, pitch) = if ten_bit {
            let gray: Vec<u8> = std::iter::repeat_n(0x8000u16.to_le_bytes(), samples).flatten().collect();
            (DXGI_FORMAT_P010, gray, width * 2)
        } else {
            (DXGI_FORMAT_NV12, vec![128u8; samples], width)
        };
        let desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        let init = D3D11_SUBRESOURCE_DATA { pSysMem: data.as_ptr().cast(), SysMemPitch: pitch, SysMemSlicePitch: 0 };
        let mut texture = None;
        // SAFETY: `desc` and `init` describe `data`, which holds the luma rows followed by the
        // chroma rows at `pitch` bytes each and outlives the call; `texture` is a valid out-pointer.
        unsafe { device.CreateTexture2D(&desc, Some(&init), Some(&mut texture)) }.map_err(|e| e.to_string())?;
        texture.ok_or_else(|| "CreateTexture2D returned no texture".into())
    }

    fn nvidia_device() -> Result<ID3D11Device, String> {
        // SAFETY: COM and Direct3D calls with valid out-pointers; each adapter outlives the
        // `D3D11CreateDevice` call it is passed to.
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(|e| format!("no DXGI: {e}"))?;
            let mut i = 0;
            while let Ok(adapter) = factory.EnumAdapters1(i) {
                i += 1;
                let Ok(desc) = adapter.GetDesc1() else { continue };
                if desc.VendorId != NVIDIA {
                    continue;
                }
                let mut device = None;
                let adapter: IDXGIAdapter = adapter.cast().map_err(|e| e.to_string())?;
                if D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_FLAG(0),
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    None,
                )
                .is_ok()
                    && let Some(d) = device
                {
                    return Ok(d);
                }
            }
        }
        Err("no NVIDIA adapter".into())
    }
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
mod cuda {
    use super::c_void;

    type Init = unsafe extern "C" fn(u32) -> i32;
    type GetDevice = unsafe extern "C" fn(*mut i32, i32) -> i32;
    type Retain = unsafe extern "C" fn(*mut *mut c_void, i32) -> i32;
    type Release = unsafe extern "C" fn(i32) -> i32;

    /// One retained primary CUDA context. Retaining it does not change the calling thread's
    /// current context; the encoder receives the context handle itself.
    pub struct Device {
        context: *mut c_void,
        ordinal: i32,
        release: Release,
        _library: libloading::Library,
    }

    impl Device {
        pub const KIND: u32 = super::super::abi::NV_ENC_DEVICE_TYPE_CUDA;

        pub fn new() -> Result<Self, String> {
            // SAFETY: the CUDA driver API signatures of NVIDIA's public Driver API reference. Every
            // symbol is resolved before the context is retained, and the library stays loaded in
            // the device until after the release.
            unsafe {
                let library = libloading::Library::new("libcuda.so.1").map_err(|e| format!("no NVIDIA CUDA driver: {e}"))?;
                let init: libloading::Symbol<Init> = library.get(b"cuInit\0").map_err(|e| e.to_string())?;
                let get: libloading::Symbol<GetDevice> = library.get(b"cuDeviceGet\0").map_err(|e| e.to_string())?;
                let retain: libloading::Symbol<Retain> = library.get(b"cuDevicePrimaryCtxRetain\0").map_err(|e| e.to_string())?;
                let release: Release = *library.get(b"cuDevicePrimaryCtxRelease_v2\0").map_err(|e| e.to_string())?;
                let st = init(0);
                if st != 0 {
                    return Err(format!("cuInit failed ({st})"));
                }
                let mut ordinal = 0;
                let st = get(&mut ordinal, 0);
                if st != 0 {
                    return Err(format!("no CUDA device ({st})"));
                }
                let mut context = std::ptr::null_mut();
                let st = retain(&mut context, ordinal);
                if st != 0 {
                    return Err(format!("cuDevicePrimaryCtxRetain failed ({st})"));
                }
                if context.is_null() {
                    let _ = release(ordinal);
                    return Err("CUDA returned a null primary context".into());
                }
                Ok(Self { context, ordinal, release, _library: library })
            }
        }

        pub fn as_raw(&self) -> *mut c_void {
            self.context
        }
    }

    impl Drop for Device {
        fn drop(&mut self) {
            // SAFETY: balances this device's one successful retain, after the session that used it
            // is destroyed. The release drops this reference only.
            let st = unsafe { (self.release)(self.ordinal) };
            if st != 0 {
                log::warn!("cuDevicePrimaryCtxRelease failed ({st})");
            }
        }
    }
}
