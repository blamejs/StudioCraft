//! `amfrt64.dll`, loaded from the system directory and initialized once per process, and one
//! hardware encoder component.
//!
//! - Each encoder has its own AMF context, which opens a Direct3D 11 device of its own.
//! - Frames go in as NV12 surfaces in system memory that AMF allocates; the encoder uploads them.
//! - Each packet is read from the `AMFBuffer` that `QueryOutput` returns, with the time its frame
//!   went in with.

use std::sync::{Mutex, OnceLock, PoisonError};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
use windows::core::{PCSTR, w};

use super::abi::*;
use super::{Engine, Need, OpenError, Output, Params, Polled, Submitted, Value, av1_default_alignment_fits, max_packet, refusal, result_name};
use crate::Codec;

/// The factory `AMFInit` returned. Calls on it go through the mutex.
struct Factory(*mut AMFFactory);

// SAFETY: the factory is a process-wide object of the AMF runtime that lives until the process
// ends; this crate calls it only while holding the mutex around it.
unsafe impl Send for Factory {}

/// The AMF runtime, loaded once for the process. It is never unloaded.
struct Api {
    factory: Mutex<Factory>,
}

fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(load).as_ref().map_err(Clone::clone)
}

/// Whether `amfrt64.dll` loaded and `AMFInit` returned a factory.
pub fn loaded() -> Result<(), String> {
    api().map(drop)
}

/// The exported function `name` of `module` as a function pointer of type `F`.
///
/// # Safety
/// `F` must be the `extern "C" fn` type that `name` really has.
unsafe fn symbol<F: Copy>(module: HMODULE, name: PCSTR) -> Result<F, String> {
    if std::mem::size_of::<F>() != std::mem::size_of::<usize>() {
        return Err("not a function pointer type".into());
    }
    // SAFETY: `module` is a loaded module handle and `name` a NUL-terminated string literal.
    let p = unsafe { GetProcAddress(module, name) }.ok_or_else(|| "amfrt64.dll lacks an expected function".to_string())?;
    // SAFETY: `F` is pointer-sized (checked above) and, by this function's contract, the type of
    // the exported function `p` points to.
    Ok(unsafe { std::mem::transmute_copy::<_, F>(&p) })
}

fn load() -> Result<Api, String> {
    // SAFETY: loads a DLL by name from the system directory only.
    let module = unsafe { LoadLibraryExW(w!("amfrt64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }
        .map_err(|_| "AMD's AMF runtime (amfrt64.dll) is not installed; it comes with AMD's graphics driver".to_string())?;
    // SAFETY: each symbol is looked up with the signature `core/Factory.h` declares for it.
    let (query, init) = unsafe {
        (
            symbol::<AMFQueryVersion_Fn>(module, PCSTR(c"AMFQueryVersion".as_ptr().cast()))?,
            symbol::<AMFInit_Fn>(module, PCSTR(c"AMFInit".as_ptr().cast()))?,
        )
    };
    let mut runtime = 0u64;
    // SAFETY: `runtime` is a valid out-pointer for the call.
    let r = unsafe { query(&mut runtime) };
    if r != AMF_OK {
        return Err(format!("AMFQueryVersion failed ({})", result_name(r)));
    }
    let version = super::init_version(runtime)?;
    let mut factory = std::ptr::null_mut();
    // SAFETY: `factory` is a valid out-pointer, and `version` is at most the runtime's own.
    let r = unsafe { init(version, &mut factory) };
    if r != AMF_OK {
        return Err(format!("AMFInit failed ({})", result_name(r)));
    }
    if factory.is_null() {
        return Err("AMFInit returned no factory".into());
    }
    Ok(Api { factory: Mutex::new(Factory(factory)) })
}

/// `s` as a NUL-terminated UTF-16 string.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn variant(v: Value) -> AMFVariantStruct {
    match v {
        Value::Int(i) => AMFVariantStruct::int64(i),
        Value::Bool(b) => AMFVariantStruct::bool(b),
        Value::Size(w, h) => AMFVariantStruct::size(variant_int(w), variant_int(h)),
        Value::Rate(n, d) => AMFVariantStruct::rate(n, d),
    }
}

/// The bytes of `buffer`, at most `limit` of them.
///
/// # Safety
/// `buffer` must be a live `AMFBuffer`.
unsafe fn buffer_bytes(buffer: *mut AMFBuffer, limit: usize) -> Result<Vec<u8>, String> {
    // SAFETY: by the contract, `buffer` is live, so its table is the `AMFBuffer` table.
    let (size, native) = unsafe { (((*(*buffer).pVtbl).GetSize)(buffer), ((*(*buffer).pVtbl).GetNative)(buffer)) };
    if size > limit {
        return Err(format!("the encoder returned a buffer of {size} bytes, more than {limit}"));
    }
    if size == 0 {
        return Ok(Vec::new());
    }
    if native.is_null() {
        return Err("the encoder returned a buffer without memory".into());
    }
    // SAFETY: a host `AMFBuffer`'s native pointer addresses `size` readable bytes while the
    // buffer is alive, which the caller's reference guarantees.
    Ok(unsafe { std::slice::from_raw_parts(native.cast::<u8>(), size) }.to_vec())
}

/// The `AMFBuffer` interface of `object`, as a new reference, or `None` when it has none.
///
/// # Safety
/// `object` must be a live AMF object.
unsafe fn as_buffer(object: *mut AMFInterface) -> Option<*mut AMFBuffer> {
    let mut out = std::ptr::null_mut();
    // SAFETY: by the contract, `object` is live; every AMF object starts with the
    // `AMFInterface` entries, and `out` is a valid out-pointer.
    let r = unsafe { ((*(*object).pVtbl).QueryInterface)(object, &IID_AMF_BUFFER, &mut out) };
    (r == AMF_OK && !out.is_null()).then_some(out.cast())
}

/// Release one reference to `object`.
///
/// # Safety
/// `object` must be a live AMF object that the caller holds a reference to, which this gives up.
unsafe fn release(object: *mut AMFInterface) {
    // SAFETY: by the contract.
    unsafe { ((*(*object).pVtbl).Release)(object) };
}

/// One AMF hardware encoder, initialized for its stream.
pub struct Session {
    context: *mut AMFContext,
    component: *mut AMFComponent,
    codec: Codec,
    width: u32,
    height: u32,
}

// SAFETY: the context and component belong to this session alone and are only used through
// `&mut self` (or dropped), one thread at a time; AMF objects are not tied to the thread that made
// them.
unsafe impl Send for Session {}

impl Session {
    /// Open and initialize an encoder for `p`.
    pub fn open(p: &Params) -> Result<Session, OpenError> {
        let api = api().map_err(OpenError::NoDevice)?;
        let factory = api.factory.lock().unwrap_or_else(PoisonError::into_inner);
        let mut context = std::ptr::null_mut();
        // SAFETY: the factory is the live one `AMFInit` returned, and `context` a valid
        // out-pointer.
        let r = unsafe { ((*(*factory.0).pVtbl).CreateContext)(factory.0, &mut context) };
        if r != AMF_OK {
            return Err(refusal("CreateContext", r));
        }
        if context.is_null() {
            return Err(OpenError::Failed("CreateContext returned no context".into()));
        }
        // From here on, dropping `session` terminates and releases what was made.
        let mut session = Session { context, component: std::ptr::null_mut(), codec: p.codec, width: p.width, height: p.height };
        // SAFETY: `context` is live; a null device asks AMF to open its own Direct3D 11 device.
        let r = unsafe { ((*(*context).pVtbl).InitDX11)(context, std::ptr::null_mut(), AMF_DX11_0) };
        if r != AMF_OK {
            return Err(OpenError::NoDevice(format!("InitDX11 failed ({}): AMF found no AMD GPU to encode on", result_name(r))));
        }
        let id = wide(super::component_id(p.codec));
        let mut component = std::ptr::null_mut();
        // SAFETY: the factory and `context` are live, `id` is NUL-terminated and outlives the
        // call, and `component` is a valid out-pointer.
        let r = unsafe { ((*(*factory.0).pVtbl).CreateComponent)(factory.0, context, id.as_ptr(), &mut component) };
        drop(factory);
        if r != AMF_OK {
            return Err(refusal(&format!("CreateComponent({})", super::component_id(p.codec)), r));
        }
        if component.is_null() {
            return Err(OpenError::Failed(format!("CreateComponent({}) returned no component", super::component_id(p.codec))));
        }
        session.component = component;
        for (name, value, need) in super::properties(p) {
            let r = session.set(name, value);
            if r == AMF_OK {
                continue;
            }
            if name == "Av1AlignmentMode" && !av1_default_alignment_fits(p.width, p.height) {
                return Err(OpenError::Unsupported(format!(
                    "{}x{}: this AV1 encoder takes only frames in 64 × 16 blocks ({})",
                    p.width,
                    p.height,
                    result_name(r)
                )));
            }
            if need == Need::Required {
                return Err(refusal(&format!("setting {name}"), r));
            }
        }
        // SAFETY: `component` is live and set up; the format is NV12 at the stream's size.
        let r = unsafe { ((*(*component).pVtbl).Init)(component, AMF_SURFACE_NV12, variant_int(p.width), variant_int(p.height)) };
        if r != AMF_OK {
            return Err(refusal("Init", r));
        }
        Ok(session)
    }

    fn set(&mut self, name: &str, value: Value) -> AMF_RESULT {
        let name = wide(name);
        // SAFETY: the component is live, `name` is NUL-terminated and outlives the call, and the
        // variant holds no pointer.
        unsafe { ((*(*self.component).pVtbl).storage.SetProperty)(self.component, name.as_ptr(), variant(value)) }
    }

    /// The parameter sets (AV1: the sequence header) the encoder reports after `Init`, when it
    /// reports them.
    pub fn extra_data(&self) -> Option<Vec<u8>> {
        let name = wide(super::extra_data(self.codec));
        let mut v = AMFVariantStruct::empty();
        // SAFETY: the component is live, `name` is NUL-terminated, and `v` is a valid
        // out-pointer.
        let r = unsafe { ((*(*self.component).pVtbl).storage.GetProperty)(self.component, name.as_ptr(), &mut v) };
        if r != AMF_OK || v.type_ != AMF_VARIANT_INTERFACE {
            return None;
        }
        // SAFETY: the variant's type says its value is an interface pointer.
        let object = unsafe { v.value.pInterface };
        if object.is_null() {
            return None;
        }
        // SAFETY: `object` is the live object the variant holds a reference to.
        let bytes = unsafe { as_buffer(object) }.and_then(|buffer| {
            // SAFETY: `buffer` is a live `AMFBuffer` this side holds a new reference to, given
            // up right after the copy.
            let bytes = unsafe { buffer_bytes(buffer, 1 << 16) };
            // SAFETY: as above.
            unsafe { release(buffer.cast()) };
            bytes.ok()
        });
        // SAFETY: a variant that `GetProperty` filled holds a reference to its interface, which
        // the caller gives up when done with it.
        unsafe { release(object) };
        bytes.filter(|b| !b.is_empty())
    }
}

/// `x` as AMF's signed size, clamped; `check` refuses larger streams before an encoder opens.
fn variant_int(x: u32) -> i32 {
    i32::try_from(x).unwrap_or(i32::MAX)
}

/// Copy the tightly packed NV12 `frame` into the planes of the host surface `surface`.
///
/// # Safety
/// `surface` must be a live host-memory NV12 surface.
unsafe fn fill(surface: *mut AMFSurface, frame: &[u8], width: u32, height: u32) -> Result<(), String> {
    let (w, h) = (width as usize, height as usize);
    if frame.len() != w * h * 3 / 2 {
        return Err(format!("a frame of {} bytes is not an NV12 frame of {width}x{height}", frame.len()));
    }
    let (luma, chroma) = frame.split_at(w * h);
    for (plane, src, rows) in [(AMF_PLANE_Y, luma, h), (AMF_PLANE_UV, chroma, h / 2)] {
        // SAFETY: by the contract, `surface` is live, so its table is the `AMFSurface` table.
        let p = unsafe { ((*(*surface).pVtbl).GetPlane)(surface, plane) };
        if p.is_null() {
            return Err("the surface has no NV12 plane".into());
        }
        // SAFETY: `p` is a plane of the live surface, valid while the surface is.
        let (native, pitch, plane_rows) = unsafe { (((*(*p).pVtbl).GetNative)(p), ((*(*p).pVtbl).GetHPitch)(p), ((*(*p).pVtbl).GetHeight)(p)) };
        let (pitch, len) = super::plane_extent(w, rows, pitch, plane_rows)?;
        if native.is_null() {
            return Err("the surface plane has no memory".into());
        }
        // SAFETY: a host surface's plane memory holds `pitch` bytes for each of its at least
        // `rows` rows while the surface is alive; `len` covers only the frame's rows, with the
        // pitch bounded by `plane_extent`, and nothing else writes the memory before the surface
        // is submitted.
        let dst = unsafe { std::slice::from_raw_parts_mut(native.cast::<u8>(), len) };
        super::copy_plane(src, w, rows, dst, pitch)?;
    }
    Ok(())
}

impl Engine for Session {
    fn submit(&mut self, frame: &[u8], time: i64, duration: i64) -> Result<Submitted, String> {
        let mut surface = std::ptr::null_mut();
        // SAFETY: the context is live, and `surface` a valid out-pointer.
        let r = unsafe {
            ((*(*self.context).pVtbl).AllocSurface)(
                self.context,
                AMF_MEMORY_HOST,
                AMF_SURFACE_NV12,
                variant_int(self.width),
                variant_int(self.height),
                &mut surface,
            )
        };
        if r != AMF_OK {
            return Err(format!("AllocSurface failed ({})", result_name(r)));
        }
        if surface.is_null() {
            return Err("AllocSurface returned no surface".into());
        }
        // SAFETY: `surface` is the live host NV12 surface just allocated.
        let filled = unsafe { fill(surface, frame, self.width, self.height) };
        let r = filled.map(|()| {
            // SAFETY: `surface` is live; an `AMFSurface` is an `AMFData`, so passing it as one is
            // what the C interface does, and the component takes its own reference when it
            // keeps the frame.
            unsafe {
                ((*(*surface).pVtbl).data.SetPts)(surface, time);
                ((*(*surface).pVtbl).data.SetDuration)(surface, duration);
                ((*(*self.component).pVtbl).SubmitInput)(self.component, surface.cast())
            }
        });
        // SAFETY: this side's reference from `AllocSurface` is given up.
        unsafe { release(surface.cast()) };
        match r? {
            AMF_OK | AMF_NEED_MORE_INPUT => Ok(Submitted::Taken),
            AMF_INPUT_FULL => Ok(Submitted::Full),
            r => Err(format!("SubmitInput failed ({})", result_name(r))),
        }
    }

    fn poll(&mut self) -> Result<Polled, String> {
        let mut data = std::ptr::null_mut();
        // SAFETY: the component is live, and `data` a valid out-pointer.
        let r = unsafe { ((*(*self.component).pVtbl).QueryOutput)(self.component, &mut data) };
        match r {
            AMF_REPEAT => return Ok(Polled::Empty),
            AMF_EOF => return Ok(Polled::Ended),
            AMF_OK if data.is_null() => return Ok(Polled::Empty),
            AMF_OK => {}
            r => return Err(format!("QueryOutput failed ({})", result_name(r))),
        }
        // SAFETY: `data` is the live output `QueryOutput` handed over with a reference.
        let time = unsafe { ((*(*data).pVtbl).data.GetPts)(data) };
        // SAFETY: as above.
        let read = match unsafe { as_buffer(data.cast()) } {
            Some(buffer) => {
                // SAFETY: `buffer` is a live `AMFBuffer` with a new reference, given up after the
                // copy.
                let bytes = unsafe { buffer_bytes(buffer, max_packet(self.width, self.height)) };
                // SAFETY: as above.
                unsafe { release(buffer.cast()) };
                bytes
            }
            None => Err("the encoder returned an output that is not a buffer".into()),
        };
        // SAFETY: the reference `QueryOutput` handed over is given up.
        unsafe { release(data.cast()) };
        Ok(Polled::Output(Output { data: read?, time: Some(time) }))
    }

    fn drain(&mut self) -> Result<Submitted, String> {
        // SAFETY: the component is live.
        match unsafe { ((*(*self.component).pVtbl).Drain)(self.component) } {
            AMF_OK => Ok(Submitted::Taken),
            AMF_INPUT_FULL => Ok(Submitted::Full),
            r => Err(format!("Drain failed ({})", result_name(r))),
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if !self.component.is_null() {
            // SAFETY: the component is live and this session's reference to it ends here.
            unsafe {
                ((*(*self.component).pVtbl).Terminate)(self.component);
                release(self.component.cast());
            }
        }
        // SAFETY: the context is live, its component is gone, and this session's reference to it
        // ends here.
        unsafe {
            ((*(*self.context).pVtbl).Terminate)(self.context);
            release(self.context.cast());
        }
    }
}
