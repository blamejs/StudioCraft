//! `mfplat.dll`, loaded from the system directory at run time and started once per process, and
//! COM set up on each thread that uses an encoder. Windows "N" editions ship without the Media
//! Feature Pack: on them loading fails with an error and the program still starts.

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::Win32::Foundation::{HMODULE, RPC_E_CHANGED_MODE};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaBuffer, IMFMediaType, IMFSample, MF_VERSION, MFMediaType_Video, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE,
    MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute, MFT_REGISTER_TYPE_INFO, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows::core::{GUID, HRESULT, Interface, PCSTR, PWSTR, w};

type StartupFn = unsafe extern "system" fn(u32, u32) -> HRESULT;
type CreateFn = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
type CreateBufferFn = unsafe extern "system" fn(u32, *mut *mut c_void) -> HRESULT;
type CreateAlignedBufferFn = unsafe extern "system" fn(u32, u32, *mut *mut c_void) -> HRESULT;
type EnumFn =
    unsafe extern "system" fn(GUID, u32, *const MFT_REGISTER_TYPE_INFO, *const MFT_REGISTER_TYPE_INFO, *mut *mut *mut c_void, *mut u32) -> HRESULT;

/// The `mfplat.dll` entry points the encoders use, resolved at run time.
pub struct Api {
    create_media_type: CreateFn,
    create_sample: CreateFn,
    create_memory_buffer: CreateBufferFn,
    create_aligned_memory_buffer: CreateAlignedBufferFn,
    enum_transforms: EnumFn,
}

/// The exported function `name` of `module` as a function pointer of type `F`.
///
/// # Safety
/// `F` must be the `extern "system" fn` type that `name` really has.
unsafe fn symbol<F: Copy>(module: HMODULE, name: PCSTR) -> Result<F, String> {
    if std::mem::size_of::<F>() != std::mem::size_of::<usize>() {
        return Err("not a function pointer type".into());
    }
    // SAFETY: `module` is a loaded module handle and `name` a NUL-terminated string literal.
    let p = unsafe { GetProcAddress(module, name) }.ok_or_else(|| "mfplat.dll lacks an expected function".to_string())?;
    // SAFETY: `F` is pointer-sized (checked above) and, by this function's contract, the type of
    // the exported function `p` points to.
    Ok(unsafe { std::mem::transmute_copy::<_, F>(&p) })
}

/// Media Foundation, loaded and started once for the process. It is never shut down: encoders
/// come and go, and the threads of its work queues end with the process.
pub fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(load).as_ref().map_err(Clone::clone)
}

/// Whether `mfplat.dll` loaded and started.
pub fn loaded() -> Result<(), String> {
    api().map(drop)
}

fn load() -> Result<Api, String> {
    ensure_com()?;
    // SAFETY: loads a system DLL by name from the system directory only.
    let module = unsafe { LoadLibraryExW(w!("mfplat.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .map_err(|_| "Media Foundation is not installed".to_string())?;
    // SAFETY: each symbol is looked up with the signature Microsoft documents for it
    // (MFStartup, MFCreateMediaType, MFCreateSample, MFCreateMemoryBuffer,
    // MFCreateAlignedMemoryBuffer, MFTEnumEx).
    let (startup, api) = unsafe {
        (
            symbol::<StartupFn>(module, PCSTR(c"MFStartup".as_ptr().cast()))?,
            Api {
                create_media_type: symbol(module, PCSTR(c"MFCreateMediaType".as_ptr().cast()))?,
                create_sample: symbol(module, PCSTR(c"MFCreateSample".as_ptr().cast()))?,
                create_memory_buffer: symbol(module, PCSTR(c"MFCreateMemoryBuffer".as_ptr().cast()))?,
                create_aligned_memory_buffer: symbol(module, PCSTR(c"MFCreateAlignedMemoryBuffer".as_ptr().cast()))?,
                enum_transforms: symbol(module, PCSTR(c"MFTEnumEx".as_ptr().cast()))?,
            },
        )
    };
    // SAFETY: MFSTARTUP_FULL (0) with the version this binding was built for.
    let hr = unsafe { startup(MF_VERSION, 0) };
    if hr.is_err() {
        return Err(format!("MFStartup failed ({hr:?})"));
    }
    Ok(api)
}

thread_local! {
    /// COM is set up (multithreaded) once per thread that touches an encoder, and never torn down:
    /// the threads end with the process.
    static COM: Result<(), String> = init_com();
}

fn init_com() -> Result<(), String> {
    // SAFETY: CoInitializeEx has no pointer arguments; calling it on a thread that already set up
    // COM (in either apartment model) is allowed and reported through the HRESULT.
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    // A thread already in a single-threaded apartment (the UI thread) keeps it: the encoder MFTs
    // are free-threaded and work from there too.
    if hr.is_ok() || hr == RPC_E_CHANGED_MODE { Ok(()) } else { Err(format!("COM setup failed ({hr:?})")) }
}

/// Make sure the calling thread can use COM objects.
pub fn ensure_com() -> Result<(), String> {
    COM.with(Clone::clone)
}

/// An out-pointer COM creation function's result as an interface.
fn made<T: Interface>(hr: HRESULT, raw: *mut c_void, what: &str) -> Result<T, String> {
    if hr.is_err() || raw.is_null() {
        return Err(format!("{what} failed ({hr:?})"));
    }
    // SAFETY: the creation function succeeded, so `raw` is a new reference to an object of type
    // `T` that this takes ownership of.
    Ok(unsafe { T::from_raw(raw) })
}

impl Api {
    pub fn media_type(&self) -> Result<IMFMediaType, String> {
        let mut raw = std::ptr::null_mut();
        // SAFETY: the out-pointer refers to a live local.
        let hr = unsafe { (self.create_media_type)(&mut raw) };
        made(hr, raw, "MFCreateMediaType")
    }

    /// A sample holding one empty buffer of `len` bytes, aligned as the `MFCreateAlignedMemoryBuffer`
    /// constant `align` says when there is one.
    pub fn empty_sample(&self, len: u32, align: Option<u32>) -> Result<(IMFSample, IMFMediaBuffer), String> {
        let mut raw = std::ptr::null_mut();
        let buffer: IMFMediaBuffer = match align {
            Some(a) => {
                // SAFETY: the out-pointer refers to a live local, and `a` is one of the documented
                // alignment constants (`alignment_flag`).
                let hr = unsafe { (self.create_aligned_memory_buffer)(len, a, &mut raw) };
                made(hr, raw, "MFCreateAlignedMemoryBuffer")?
            }
            None => {
                // SAFETY: the out-pointer refers to a live local.
                let hr = unsafe { (self.create_memory_buffer)(len, &mut raw) };
                made(hr, raw, "MFCreateMemoryBuffer")?
            }
        };
        let mut raw = std::ptr::null_mut();
        // SAFETY: the out-pointer refers to a live local.
        let hr = unsafe { (self.create_sample)(&mut raw) };
        let sample: IMFSample = made(hr, raw, "MFCreateSample")?;
        // SAFETY: a plain COM call on live interfaces.
        unsafe { sample.AddBuffer(&buffer) }.map_err(|e| format!("cannot add a buffer to a sample: {e}"))?;
        Ok((sample, buffer))
    }

    /// An input sample holding a copy of `data`, at `time` for `duration` (100 ns units).
    pub fn sample_of(&self, data: &[u8], time: i64, duration: i64) -> Result<IMFSample, String> {
        let len = u32::try_from(data.len()).map_err(|_| "a frame is too large".to_string())?;
        let (sample, buffer) = self.empty_sample(len, None)?;
        // SAFETY: `Lock` gives a writable block of at least `max` bytes valid until `Unlock`; the
        // copy stays within `len <= max` bytes and the buffer is unlocked before it is used.
        unsafe {
            let (mut ptr, mut max) = (std::ptr::null_mut::<u8>(), 0u32);
            buffer.Lock(&mut ptr, Some(&mut max), None).map_err(|e| format!("cannot lock the input buffer: {e}"))?;
            if ptr.is_null() || max < len {
                let _ = buffer.Unlock();
                return Err("the input buffer is too small".into());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            buffer.Unlock().map_err(|e| format!("cannot unlock the input buffer: {e}"))?;
            buffer.SetCurrentLength(len).map_err(|e| format!("cannot size the input buffer: {e}"))?;
            sample.SetSampleTime(time).map_err(|e| format!("cannot timestamp the input: {e}"))?;
            sample.SetSampleDuration(duration).map_err(|e| format!("cannot set the input's duration: {e}"))?;
        }
        Ok(sample)
    }

    /// The hardware encoder MFTs that take NV12 and write `subtype`, in the order Media Foundation
    /// sorts them, as activation objects.
    pub fn encoders(&self, subtype: GUID) -> Result<Vec<IMFActivate>, String> {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: subtype };
        let flags = (MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0) as u32;
        let (mut list, mut count) = (std::ptr::null_mut::<*mut c_void>(), 0u32);
        // SAFETY: the type infos and the out-pointers refer to live locals.
        let hr = unsafe { (self.enum_transforms)(MFT_CATEGORY_VIDEO_ENCODER, flags, &input, &output, &mut list, &mut count) };
        if hr.is_err() {
            return Err(format!("MFTEnumEx failed ({hr:?})"));
        }
        let mut out = Vec::new();
        if !list.is_null() {
            for i in 0..count as usize {
                // SAFETY: `list` holds `count` pointers in memory allocated with CoTaskMemAlloc, each
                // null or a reference to an `IMFActivate` that this takes over.
                unsafe {
                    let p = *list.add(i);
                    if !p.is_null() {
                        out.push(IMFActivate::from_raw(p));
                    }
                }
            }
            // SAFETY: the array itself was allocated by MFTEnumEx with CoTaskMemAlloc.
            unsafe { CoTaskMemFree(Some(list as *const c_void)) };
        }
        Ok(out)
    }
}

/// The name an MFT registered.
pub fn friendly_name(a: &IMFActivate) -> Option<String> {
    // SAFETY: a plain COM call; `GetAllocatedString` returns a CoTaskMem string that the returned
    // `PWSTR` wrapper does not free, so it is freed here after copying.
    unsafe {
        let mut s = PWSTR::null();
        let mut len = 0u32;
        a.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut s, &mut len).ok()?;
        let name = s.to_string().ok();
        CoTaskMemFree(Some(s.0 as *const c_void));
        name
    }
}
