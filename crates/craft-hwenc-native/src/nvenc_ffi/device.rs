//! The device an encode session is opened on: a Direct3D 11 device of the first NVIDIA adapter on
//! Windows, the retained primary CUDA context of device 0 on Linux. The handles stay in this module.

use std::ffi::c_void;

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
pub use cuda::Device;
#[cfg(target_os = "windows")]
pub use directx::Device;
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

        pub fn as_raw(&self) -> *mut c_void {
            self.0.as_raw()
        }
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
