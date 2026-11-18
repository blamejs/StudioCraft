//! An NVENC encode session: the driver library, the device it is opened on, the encoder
//! configuration and the input and output buffers. Every call into the driver is in this file; the
//! items it exports are safe and return `Result<_, String>`, and no driver pointer leaves it.

use std::ffi::{CStr, c_void};
use std::sync::OnceLock;

use super::abi::*;
use super::device::Device;

/// The driver's entry points, loaded once.
struct Api {
    list: NV_ENCODE_API_FUNCTION_LIST,
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    _library: libloading::Library,
}

// SAFETY: the function table holds plain function pointers that do not change after loading, and
// the encode API may be called from any thread (calls on one session are serialized by `&mut`).
unsafe impl Send for Api {}
// SAFETY: see the `Send` implementation above.
unsafe impl Sync for Api {}

fn api() -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(load).as_ref().map_err(Clone::clone)
}

/// The API version a driver reports, in the form `NvEncodeAPIGetMaxSupportedVersion` returns it
/// (major in the high nibbles, minor in the low one).
fn our_version() -> u32 {
    ((NVENCAPI_VERSION & 0xf) << 4) | ((NVENCAPI_VERSION >> 24) & 0xf)
}

/// Check the driver's version and fill the function table through `create`.
///
/// # Safety
///
/// `version` and `create` are the driver's `NvEncodeAPIGetMaxSupportedVersion` and
/// `NvEncodeAPICreateInstance`.
#[cfg(any(target_os = "windows", all(target_os = "linux", target_pointer_width = "64")))]
unsafe fn function_list(version: MaxSupportedVersionFn, create: CreateInstanceFn) -> Result<NV_ENCODE_API_FUNCTION_LIST, String> {
    let mut v = 0u32;
    // SAFETY: the caller passes the driver's function; `v` is a valid out-pointer.
    if unsafe { version(&mut v) } != NV_ENC_SUCCESS {
        return Err("cannot query the NVENC version".into());
    }
    let ours = our_version();
    if v < ours {
        return Err(format!("the NVIDIA driver's NVENC ({}.{}) is older than {}.{}", v >> 4, v & 0xf, ours >> 4, ours & 0xf));
    }
    // SAFETY: all zero is a valid table (null pointers and `None`); the version is set before the
    // call, as the API asks, and the driver fills the rest.
    let mut list: NV_ENCODE_API_FUNCTION_LIST = unsafe { std::mem::zeroed() };
    list.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    // SAFETY: the caller passes the driver's function; `list` is a valid table to fill.
    if unsafe { create(&mut list) } != NV_ENC_SUCCESS {
        return Err("NvEncodeAPICreateInstance failed".into());
    }
    Ok(list)
}

#[cfg(target_os = "windows")]
fn load() -> Result<Api, String> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
    use windows::core::{PCSTR, w};
    // SAFETY: loads the driver's library from the system directory only.
    let module: HMODULE =
        unsafe { LoadLibraryExW(w!("nvEncodeAPI64.dll"), None, LOAD_LIBRARY_SEARCH_SYSTEM32) }.map_err(|_| "no NVIDIA encoder driver".to_string())?;
    // SAFETY: both symbols are looked up by name in the driver's library and converted to the
    // signatures `nvEncodeAPI.h` gives them; the library stays loaded for the life of the process.
    unsafe {
        let version: MaxSupportedVersionFn = std::mem::transmute(
            GetProcAddress(module, PCSTR(c"NvEncodeAPIGetMaxSupportedVersion".as_ptr().cast()))
                .ok_or("the driver lacks NvEncodeAPIGetMaxSupportedVersion")?,
        );
        let create: CreateInstanceFn = std::mem::transmute(
            GetProcAddress(module, PCSTR(c"NvEncodeAPICreateInstance".as_ptr().cast())).ok_or("the driver lacks NvEncodeAPICreateInstance")?,
        );
        Ok(Api { list: function_list(version, create)? })
    }
}

#[cfg(all(target_os = "linux", target_pointer_width = "64"))]
fn load() -> Result<Api, String> {
    // SAFETY: loads NVIDIA's driver library and looks up both symbols with the signatures of
    // `nvEncodeAPI.h`; the library is kept with the function table for the life of the process.
    unsafe {
        let library = libloading::Library::new("libnvidia-encode.so.1").map_err(|e| format!("no NVIDIA encoder driver: {e}"))?;
        let version: MaxSupportedVersionFn = *library.get(b"NvEncodeAPIGetMaxSupportedVersion\0").map_err(|e| e.to_string())?;
        let create: CreateInstanceFn = *library.get(b"NvEncodeAPICreateInstance\0").map_err(|e| e.to_string())?;
        let list = function_list(version, create)?;
        Ok(Api { list, _library: library })
    }
}

#[cfg(not(any(target_os = "windows", all(target_os = "linux", target_pointer_width = "64"))))]
fn load() -> Result<Api, String> {
    Err("NVENC runs on Windows and 64-bit Linux".into())
}

/// The codec a session encodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    H264,
    Hevc,
}

impl Codec {
    fn guid(self) -> Guid {
        match self {
            Codec::H264 => NV_ENC_CODEC_H264_GUID,
            Codec::Hevc => NV_ENC_CODEC_HEVC_GUID,
        }
    }
}

/// What the encoder reports it can do for a codec.
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    /// 10-bit encoding is supported.
    pub ten_bit: bool,
    pub min_size: (u32, u32),
    pub max_size: (u32, u32),
}

/// How the session spends bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rate {
    /// Constant quality at this level (1 to 51, lower is better).
    Quality(u32),
    /// Constant bitrate in kbit/s, with a one-second buffer.
    Cbr(u32),
}

/// Encoder settings, checked by the caller.
#[derive(Clone, Debug)]
pub struct Params {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: (u32, u32),
    pub rate: Rate,
    /// Pictures from one IDR picture to the next.
    pub gop: u32,
    /// HEVC Main 10 from P010 pictures; otherwise 8-bit (H.264 High or HEVC Main) from NV12.
    pub ten_bit: bool,
}

/// An input buffer locked for writing: luma rows at `pitch` bytes per row, then the interleaved
/// chroma rows after `pitch * height` bytes.
pub struct Locked<'a> {
    pub data: &'a mut [u8],
    pub pitch: usize,
}

/// A finished picture.
pub struct Output {
    /// Annex B bytes.
    pub data: Vec<u8>,
    pub pts: u64,
    /// Whether it is an IDR picture.
    pub key: bool,
}

/// The result of submitting a picture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Submitted {
    /// The pictures submitted so far have their bitstreams ready.
    Ready,
    /// The encoder needs more pictures before it has output.
    NeedMoreInput,
}

struct Slot {
    input: *mut c_void,
    output: *mut c_void,
}

/// An encode session with its buffers.
pub struct Session {
    api: &'static Api,
    enc: Handle,
    // Keeps the Direct3D device or the retained CUDA context alive until the session is destroyed.
    _device: Device,
    slots: Vec<Slot>,
    size: (u32, u32),
    out_size: u32,
    /// `NV_ENC_BUFFER_FORMAT_*` of the input buffers, and its bytes per sample.
    fmt: u32,
    bytes_per_sample: usize,
}

// SAFETY: a session and its buffers are driver objects used from one thread at a time (every call
// takes `&mut self` or `&self` of an owned session); Direct3D 11 devices are free-threaded, and the
// encoder takes the CUDA context explicitly.
unsafe impl Send for Session {}

/// A readable form of a driver status.
fn status(what: &str, s: NvStatus, enc: Handle, api: &Api) -> String {
    let detail = (!enc.is_null())
        .then(|| {
            // SAFETY: a valid session handle; the driver returns a NUL-terminated string it owns,
            // which is copied before the next call.
            api.list.nvEncGetLastErrorString.and_then(|f| unsafe {
                let p = f(enc);
                (!p.is_null()).then(|| CStr::from_ptr(p).to_string_lossy().into_owned())
            })
        })
        .flatten()
        .filter(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_graphic() || c == ' '));
    match detail {
        Some(d) => format!("{what} failed ({s}): {d}"),
        None => format!("{what} failed ({s})"),
    }
}

/// Whether the system has the NVIDIA driver's encode library in a version this crate speaks.
pub fn driver() -> Result<(), String> {
    api().map(|_| ())
}

/// Why a session did not open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// No NVIDIA GPU, driver or encode library.
    NoDevice(String),
    /// The GPU has no free encoding session; another program holds them.
    Busy(String),
}

/// Whether `s`, returned by `nvEncOpenEncodeSessionEx`, means every session of the GPU is taken.
/// The driver gives `NV_ENC_ERR_OUT_OF_MEMORY` for a full session count, and
/// `NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY` or `NV_ENC_ERR_ENCODER_BUSY` on some drivers.
pub(crate) fn busy(s: NvStatus) -> bool {
    matches!(s, NV_ENC_ERR_OUT_OF_MEMORY | NV_ENC_ERR_ENCODER_BUSY | NV_ENC_ERR_INCOMPATIBLE_CLIENT_KEY)
}

impl Session {
    /// Open a session on the system's NVIDIA GPU, or say why not.
    pub fn open() -> Result<Session, OpenError> {
        let api = api().map_err(OpenError::NoDevice)?;
        let device = Device::new().map_err(OpenError::NoDevice)?;
        let open = api.list.nvEncOpenEncodeSessionEx.ok_or_else(|| OpenError::NoDevice("the driver has no nvEncOpenEncodeSessionEx".into()))?;
        // SAFETY: a zeroed parameter block with its version and the device's pointer; the device
        // stays in the session while the encoder lives. After a failed open, a non-null handle is
        // the driver's and is destroyed here, once.
        let enc = unsafe {
            let mut p: NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS = std::mem::zeroed();
            p.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
            p.deviceType = Device::KIND;
            p.device = device.as_raw();
            p.apiVersion = NVENCAPI_VERSION;
            let mut enc: Handle = std::ptr::null_mut();
            let s = open(&mut p, &mut enc);
            if s != NV_ENC_SUCCESS || enc.is_null() {
                let e = status("opening the NVENC session", s, enc, api);
                if !enc.is_null()
                    && let Some(d) = api.list.nvEncDestroyEncoder
                {
                    d(enc);
                }
                return Err(if busy(s) { OpenError::Busy(e) } else { OpenError::NoDevice(e) });
            }
            enc
        };
        Ok(Session { api, enc, _device: device, slots: Vec::new(), size: (0, 0), out_size: 0, fmt: NV_ENC_BUFFER_FORMAT_NV12, bytes_per_sample: 1 })
    }

    /// The encoder's limits for `codec`; an error when the GPU cannot encode it.
    pub fn caps(&self, codec: Codec) -> Result<Caps, String> {
        let f = self.api.list.nvEncGetEncodeCaps.ok_or("the driver has no nvEncGetEncodeCaps")?;
        let get = |cap: u32| -> Result<u32, String> {
            // SAFETY: a valid session, a zeroed parameter block with its version and one int out.
            unsafe {
                let mut p: NV_ENC_CAPS_PARAM = std::mem::zeroed();
                p.version = NV_ENC_CAPS_PARAM_VER;
                p.capsToQuery = cap;
                let mut v = 0i32;
                let s = f(self.enc, codec.guid(), &mut p, &mut v);
                if s != NV_ENC_SUCCESS {
                    return Err(status("querying the encoder", s, self.enc, self.api));
                }
                Ok(u32::try_from(v).unwrap_or(0))
            }
        };
        Ok(Caps {
            // a failing query means no 10-bit encoder
            ten_bit: get(NV_ENC_CAPS_SUPPORT_10BIT_ENCODE).unwrap_or(0) != 0,
            min_size: (get(NV_ENC_CAPS_WIDTH_MIN)?, get(NV_ENC_CAPS_HEIGHT_MIN)?),
            max_size: (get(NV_ENC_CAPS_WIDTH_MAX)?, get(NV_ENC_CAPS_HEIGHT_MAX)?),
        })
    }

    /// Configure the encoder (preset P5, high-quality tuning, no B-frames) and create `ring` pairs
    /// of input and output buffers.
    pub fn initialize(&mut self, p: &Params, ring: usize) -> Result<(), String> {
        if p.ten_bit && p.codec != Codec::Hevc {
            return Err("10-bit encoding is HEVC Main 10 only".into());
        }
        (self.fmt, self.bytes_per_sample) = if p.ten_bit { (NV_ENC_BUFFER_FORMAT_YUV420_10BIT, 2) } else { (NV_ENC_BUFFER_FORMAT_NV12, 1) };
        let preset_fn = self.api.list.nvEncGetEncodePresetConfigEx.ok_or("the driver has no nvEncGetEncodePresetConfigEx")?;
        let init_fn = self.api.list.nvEncInitializeEncoder.ok_or("the driver has no nvEncInitializeEncoder")?;
        // SAFETY: zeroed parameter blocks with their versions; the preset call fills `preset`, and
        // the configuration lives in a box that outlives `nvEncInitializeEncoder`, which copies it.
        // The union fields written are the ones of `p.codec`, the codec the preset was read for.
        unsafe {
            let mut preset: Box<NV_ENC_PRESET_CONFIG> = Box::new(std::mem::zeroed());
            preset.version = NV_ENC_PRESET_CONFIG_VER;
            preset.presetCfg.version = NV_ENC_CONFIG_VER;
            let s = preset_fn(self.enc, p.codec.guid(), NV_ENC_PRESET_P5_GUID, NV_ENC_TUNING_INFO_HIGH_QUALITY, &mut *preset);
            if s != NV_ENC_SUCCESS {
                return Err(status("reading the encoder preset", s, self.enc, self.api));
            }
            let mut cfg: Box<NV_ENC_CONFIG> = Box::new(preset.presetCfg);
            cfg.version = NV_ENC_CONFIG_VER;
            cfg.profileGUID = match p.codec {
                Codec::Hevc if p.ten_bit => NV_ENC_HEVC_PROFILE_MAIN10_GUID,
                Codec::Hevc => NV_ENC_HEVC_PROFILE_MAIN_GUID,
                Codec::H264 => NV_ENC_H264_PROFILE_HIGH_GUID,
            };
            cfg.gopLength = p.gop.max(1);
            cfg.frameIntervalP = 1;
            let rc = &mut cfg.rcParams;
            rc.version = NV_ENC_RC_PARAMS_VER;
            match p.rate {
                Rate::Quality(q) => {
                    rc.rateControlMode = NV_ENC_PARAMS_RC_VBR;
                    rc.averageBitRate = 0;
                    rc.maxBitRate = 0;
                    rc.vbvBufferSize = 0;
                    rc.vbvInitialDelay = 0;
                    rc.targetQuality = u8::try_from(q.clamp(1, 51)).unwrap_or(51);
                    rc.targetQualityLSB = 0;
                }
                Rate::Cbr(kbps) => {
                    rc.rateControlMode = NV_ENC_PARAMS_RC_CBR;
                    rc.averageBitRate = kbps.saturating_mul(1000);
                    rc.maxBitRate = rc.averageBitRate;
                    rc.vbvBufferSize = rc.averageBitRate;
                    rc.vbvInitialDelay = rc.vbvBufferSize;
                }
            }
            match p.codec {
                Codec::H264 => {
                    let h = &mut cfg.encodeCodecConfig.h264Config;
                    h.idrPeriod = p.gop.max(1);
                    h.level = NV_ENC_LEVEL_AUTOSELECT;
                    h.flags &= !(H264_OUTPUT_AUD | H264_REPEAT_SPSPPS);
                    h.entropyCodingMode = NV_ENC_H264_ENTROPY_CODING_MODE_CABAC;
                    h.maxNumRefFrames = 0;
                    let vui = &mut h.h264VUIParameters;
                    vui.videoSignalTypePresentFlag = 1;
                    vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
                    vui.videoFullRangeFlag = 0;
                    vui.colourDescriptionPresentFlag = 1;
                    vui.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
                    vui.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
                    vui.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT709;
                    vui.timingInfoPresentFlag = 1;
                    vui.numUnitInTicks = p.fps.1;
                    vui.timeScale = p.fps.0.saturating_mul(2);
                }
                Codec::Hevc => {
                    let h = &mut cfg.encodeCodecConfig.hevcConfig;
                    h.level = NV_ENC_LEVEL_AUTOSELECT;
                    h.tier = NV_ENC_TIER_HEVC_MAIN;
                    h.minCUSize = NV_ENC_HEVC_CUSIZE_AUTOSELECT;
                    h.maxCUSize = NV_ENC_HEVC_CUSIZE_AUTOSELECT;
                    h.idrPeriod = p.gop.max(1);
                    h.flags &= !(HEVC_OUTPUT_AUD | HEVC_REPEAT_SPSPPS | HEVC_CHROMA_FORMAT_MASK | HEVC_PIXEL_BIT_DEPTH_MASK);
                    h.flags |= 1 << HEVC_CHROMA_FORMAT_SHIFT;
                    if p.ten_bit {
                        h.flags |= 2 << HEVC_PIXEL_BIT_DEPTH_SHIFT;
                    }
                    h.maxNumRefFramesInDPB = 0;
                    let vui = &mut h.hevcVUIParameters;
                    vui.videoSignalTypePresentFlag = 1;
                    vui.videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
                    vui.videoFullRangeFlag = 0;
                    vui.colourDescriptionPresentFlag = 1;
                    vui.colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT709;
                    vui.transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709;
                    vui.colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT709;
                    // the encoder derives the HEVC timing from frameRateNum and frameRateDen
                    vui.timingInfoPresentFlag = 0;
                    vui.numUnitInTicks = 0;
                    vui.timeScale = 0;
                }
            }
            let mut init: Box<NV_ENC_INITIALIZE_PARAMS> = Box::new(std::mem::zeroed());
            init.version = NV_ENC_INITIALIZE_PARAMS_VER;
            init.encodeGUID = p.codec.guid();
            init.presetGUID = NV_ENC_PRESET_P5_GUID;
            init.encodeWidth = p.width;
            init.encodeHeight = p.height;
            init.darWidth = p.width;
            init.darHeight = p.height;
            init.frameRateNum = p.fps.0;
            init.frameRateDen = p.fps.1;
            init.enablePTD = 1;
            init.encodeConfig = &mut *cfg;
            init.tuningInfo = NV_ENC_TUNING_INFO_HIGH_QUALITY;
            init.bufferFormat = self.fmt;
            let s = init_fn(self.enc, &mut *init);
            if s != NV_ENC_SUCCESS {
                return Err(status("initializing the encoder", s, self.enc, self.api));
            }
        }
        self.size = (p.width, p.height);
        let bytes = u64::from(p.width).saturating_mul(u64::from(p.height)).saturating_mul(2).saturating_mul(self.bytes_per_sample as u64);
        self.out_size = u32::try_from(bytes).unwrap_or(u32::MAX).max(1 << 20);
        for _ in 0..ring.max(1) {
            self.add_slot()?;
        }
        Ok(())
    }

    fn add_slot(&mut self) -> Result<(), String> {
        let create_in = self.api.list.nvEncCreateInputBuffer.ok_or("the driver has no nvEncCreateInputBuffer")?;
        let create_out = self.api.list.nvEncCreateBitstreamBuffer.ok_or("the driver has no nvEncCreateBitstreamBuffer")?;
        // SAFETY: zeroed parameter blocks with their versions; the driver fills in the buffer
        // handles, which `Drop` destroys. An input buffer whose output buffer fails is destroyed here.
        unsafe {
            let mut i: NV_ENC_CREATE_INPUT_BUFFER = std::mem::zeroed();
            i.version = NV_ENC_CREATE_INPUT_BUFFER_VER;
            i.width = self.size.0;
            i.height = self.size.1;
            i.memoryHeap = NV_ENC_MEMORY_HEAP_SYSMEM_CACHED;
            i.bufferFmt = self.fmt;
            let s = create_in(self.enc, &mut i);
            if s != NV_ENC_SUCCESS {
                return Err(status("creating an input buffer", s, self.enc, self.api));
            }
            let mut o: NV_ENC_CREATE_BITSTREAM_BUFFER = std::mem::zeroed();
            o.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
            o.size = self.out_size;
            o.memoryHeap = NV_ENC_MEMORY_HEAP_SYSMEM_CACHED;
            let s = create_out(self.enc, &mut o);
            if s != NV_ENC_SUCCESS {
                if let Some(d) = self.api.list.nvEncDestroyInputBuffer {
                    d(self.enc, i.inputBuffer);
                }
                return Err(status("creating an output buffer", s, self.enc, self.api));
            }
            self.slots.push(Slot { input: i.inputBuffer, output: o.bitstreamBuffer });
        }
        Ok(())
    }

    /// The number of buffer slots.
    pub fn slots(&self) -> usize {
        self.slots.len()
    }

    /// The parameter sets as one Annex B byte string (SPS and PPS; VPS, SPS and PPS for HEVC).
    pub fn sequence_params(&self) -> Result<Vec<u8>, String> {
        let f = self.api.list.nvEncGetSequenceParams.ok_or("the driver has no nvEncGetSequenceParams")?;
        let mut buf = vec![0u8; 1024];
        let mut len = 0u32;
        // SAFETY: a zeroed parameter block with its version; `buf` and `len` outlive the call, and
        // the driver writes at most `inBufferSize` bytes into `buf`.
        unsafe {
            let mut p: NV_ENC_SEQUENCE_PARAM_PAYLOAD = std::mem::zeroed();
            p.version = NV_ENC_SEQUENCE_PARAM_PAYLOAD_VER;
            p.inBufferSize = buf.len() as u32;
            p.spsppsBuffer = buf.as_mut_ptr().cast();
            p.outSPSPPSPayloadSize = &mut len;
            let s = f(self.enc, &mut p);
            if s != NV_ENC_SUCCESS {
                return Err(status("reading the parameter sets", s, self.enc, self.api));
            }
        }
        buf.truncate(len as usize);
        Ok(buf)
    }

    /// Write a picture into slot `slot`'s input buffer through `fill`, then submit it with the
    /// timestamp `pts`; the slot's output buffer receives the bitstream.
    pub fn submit(&mut self, slot: usize, pts: u64, fill: impl FnOnce(Locked<'_>)) -> Result<Submitted, String> {
        let s = self.slots.get(slot).ok_or("no such buffer slot")?;
        let (input, output) = (s.input, s.output);
        let lock = self.api.list.nvEncLockInputBuffer.ok_or("the driver has no nvEncLockInputBuffer")?;
        let unlock = self.api.list.nvEncUnlockInputBuffer.ok_or("the driver has no nvEncUnlockInputBuffer")?;
        let encode = self.api.list.nvEncEncodePicture.ok_or("the driver has no nvEncEncodePicture")?;
        let (w, h) = self.size;
        // SAFETY: the buffer handle is one this session created. A successful lock returns a
        // writable block of `pitch * h * 3 / 2` bytes (luma rows, then chroma rows, the pitch in
        // bytes) that stays valid until the unlock; the slice borrows it only inside `fill`.
        let pitch = unsafe {
            let mut l: NV_ENC_LOCK_INPUT_BUFFER = std::mem::zeroed();
            l.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
            l.inputBuffer = input;
            let st = lock(self.enc, &mut l);
            if st != NV_ENC_SUCCESS || l.bufferDataPtr.is_null() || l.pitch == 0 {
                return Err(status("locking an input buffer", st, self.enc, self.api));
            }
            if (l.pitch as usize) < (w as usize).saturating_mul(self.bytes_per_sample) {
                let _ = unlock(self.enc, input);
                return Err(format!(
                    "the driver returned an input pitch of {} bytes for {w}-pixel rows of {}-byte samples",
                    l.pitch, self.bytes_per_sample
                ));
            }
            let len = (l.pitch as usize).saturating_mul(h as usize).saturating_mul(3) / 2;
            fill(Locked { data: std::slice::from_raw_parts_mut(l.bufferDataPtr.cast::<u8>(), len), pitch: l.pitch as usize });
            let st = unlock(self.enc, input);
            if st != NV_ENC_SUCCESS {
                return Err(status("unlocking an input buffer", st, self.enc, self.api));
            }
            l.pitch
        };
        // SAFETY: a zeroed parameter block with its version; the buffers are this session's.
        let st = unsafe {
            let mut e: NV_ENC_PIC_PARAMS = std::mem::zeroed();
            e.version = NV_ENC_PIC_PARAMS_VER;
            e.inputWidth = w;
            e.inputHeight = h;
            e.inputPitch = pitch;
            e.inputBuffer = input;
            e.outputBitstream = output;
            e.bufferFmt = self.fmt;
            e.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
            e.inputTimeStamp = pts;
            e.inputDuration = 1;
            encode(self.enc, &mut e)
        };
        match st {
            NV_ENC_SUCCESS => Ok(Submitted::Ready),
            NV_ENC_ERR_NEED_MORE_INPUT => Ok(Submitted::NeedMoreInput),
            s => Err(status("encoding a picture", s, self.enc, self.api)),
        }
    }

    /// Tell the encoder the stream ends: every picture it still holds becomes ready.
    pub fn end_of_stream(&mut self) -> Result<(), String> {
        let encode = self.api.list.nvEncEncodePicture.ok_or("the driver has no nvEncEncodePicture")?;
        // SAFETY: a zeroed parameter block with its version and the end-of-stream flag, without an
        // input buffer.
        let st = unsafe {
            let mut e: NV_ENC_PIC_PARAMS = std::mem::zeroed();
            e.version = NV_ENC_PIC_PARAMS_VER;
            e.encodePicFlags = NV_ENC_PIC_FLAG_EOS;
            encode(self.enc, &mut e)
        };
        if st != NV_ENC_SUCCESS {
            return Err(status("ending the stream", st, self.enc, self.api));
        }
        Ok(())
    }

    /// Read the finished bitstream of slot `slot`, waiting until the encoder has it.
    pub fn read(&mut self, slot: usize) -> Result<Output, String> {
        let output = self.slots.get(slot).ok_or("no such buffer slot")?.output;
        let lock = self.api.list.nvEncLockBitstream.ok_or("the driver has no nvEncLockBitstream")?;
        let unlock = self.api.list.nvEncUnlockBitstream.ok_or("the driver has no nvEncUnlockBitstream")?;
        // SAFETY: a zeroed parameter block with its version and a buffer of this session; after a
        // successful lock `bitstreamBufferPtr` addresses `bitstreamSizeInBytes` readable bytes until
        // the unlock, and they are copied before it. A size larger than the buffer that was asked
        // for is refused before the slice is made.
        unsafe {
            let mut l: NV_ENC_LOCK_BITSTREAM = std::mem::zeroed();
            l.version = NV_ENC_LOCK_BITSTREAM_VER;
            l.outputBitstream = output;
            let st = lock(self.enc, &mut l);
            if st != NV_ENC_SUCCESS {
                return Err(status("reading a bitstream", st, self.enc, self.api));
            }
            let n = l.bitstreamSizeInBytes as usize;
            if n > self.out_size as usize {
                let _ = unlock(self.enc, output);
                return Err(format!("the driver reported a {n}-byte bitstream in a {}-byte buffer", self.out_size));
            }
            let data = if l.bitstreamBufferPtr.is_null() || n == 0 {
                Vec::new()
            } else {
                std::slice::from_raw_parts(l.bitstreamBufferPtr.cast::<u8>(), n).to_vec()
            };
            let out = Output { data, pts: l.outputTimeStamp, key: l.pictureType == NV_ENC_PIC_TYPE_IDR };
            let st = unlock(self.enc, output);
            if st != NV_ENC_SUCCESS {
                return Err(status("releasing a bitstream", st, self.enc, self.api));
            }
            Ok(out)
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: every handle was created by this session and is destroyed once, buffers first.
        unsafe {
            for s in self.slots.drain(..) {
                if let Some(d) = self.api.list.nvEncDestroyInputBuffer {
                    d(self.enc, s.input);
                }
                if let Some(d) = self.api.list.nvEncDestroyBitstreamBuffer {
                    d(self.enc, s.output);
                }
            }
            if let Some(d) = self.api.list.nvEncDestroyEncoder {
                d(self.enc);
            }
        }
    }
}
