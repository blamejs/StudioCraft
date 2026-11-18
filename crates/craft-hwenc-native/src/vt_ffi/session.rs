//! One VideoToolbox compression session with a hardware encoder required: NV12 pixel buffers in,
//! and the compressed samples its output callback copies out.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::panic::AssertUnwindSafe;
use std::ptr::NonNull;
use std::sync::{Mutex, PoisonError};

use objc2_core_foundation::{CFArray, CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use objc2_core_media::{
    CMSampleBuffer, CMTime, CMTimeFlags, CMVideoFormatDescriptionGetH264ParameterSetAtIndex, CMVideoFormatDescriptionGetHEVCParameterSetAtIndex,
    kCMVideoCodecType_H264, kCMVideoCodecType_HEVC,
};
use objc2_core_video::{
    CVImageBuffer, CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane, CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeightOfPlane,
    CVPixelBufferGetWidthOfPlane, CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferPool, CVPixelBufferUnlockBaseAddress,
    kCVImageBufferColorPrimaries_ITU_R_709_2, kCVImageBufferTransferFunction_ITU_R_709_2, kCVImageBufferYCbCrMatrix_ITU_R_709_2,
    kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey, kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey,
};
use objc2_video_toolbox::{
    VTCompressionSession, VTEncodeInfoFlags, VTSessionCopyProperty, VTSessionSetProperty, kVTCompressionPropertyKey_AllowFrameReordering,
    kVTCompressionPropertyKey_AllowOpenGOP, kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_ColorPrimaries,
    kVTCompressionPropertyKey_DataRateLimits, kVTCompressionPropertyKey_ExpectedFrameRate, kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_Quality, kVTCompressionPropertyKey_RealTime,
    kVTCompressionPropertyKey_TransferFunction, kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder,
    kVTCompressionPropertyKey_YCbCrMatrix, kVTProfileLevel_H264_High_AutoLevel, kVTProfileLevel_HEVC_Main_AutoLevel,
    kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,
};

use super::{MAX_FRAME_BYTES, MAX_SETS, OpenError, Output, Params, bitrate_bps, nominal_bitrate, open_error, quality, rescale};
use crate::{Codec, Rate};

/// The pixel format of the input buffers: NV12 at video range (`'420v'`).
const NV12_VIDEO: u32 = u32::from_be_bytes(*b"420v");

/// What the output callback shares with the session's owner.
struct Shared {
    codec: Codec,
    /// The session's timescale, which the samples' times are read in.
    timescale: i32,
    out: Mutex<Vec<Result<Output, String>>>,
}

impl Shared {
    fn push(&self, o: Result<Output, String>) {
        self.out.lock().unwrap_or_else(PoisonError::into_inner).push(o);
    }
}

/// A compression session that requires a hardware encoder. Dropping it completes the frames in
/// flight and invalidates the session before the callback state is freed.
pub(crate) struct Session {
    session: CFRetained<VTCompressionSession>,
    /// The output callback's state: it must outlive `session`.
    shared: Box<Shared>,
    pool: Option<CFRetained<CVPixelBufferPool>>,
    attrs: CFRetained<CFDictionary<CFString, CFType>>,
    width: usize,
    height: usize,
    timescale: i32,
}

// SAFETY: Apple documents VTCompressionSession and the CoreFoundation, CoreMedia and CoreVideo
// objects it holds as safe to use from any thread. The session's owner uses it from one thread at
// a time (`&mut self`), the buffer attributes are never changed after creation, and the callback
// state sits behind a `Mutex`.
unsafe impl Send for Session {}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: the session is valid until it is released. Completing the frames lets a callback
        // in flight finish, and after `invalidate` no callback runs, so `shared` can be freed when
        // this function returns.
        unsafe {
            let _ = self.session.complete_frames(invalid_time());
            self.session.invalidate();
        }
    }
}

/// `kCMTimeInvalid`, which `complete_frames` reads as every frame in flight.
fn invalid_time() -> CMTime {
    CMTime { value: 0, timescale: 0, flags: CMTimeFlags(0), epoch: 0 }
}

fn cm_time(value: i64, timescale: i32) -> CMTime {
    CMTime { value, timescale, flags: CMTimeFlags::Valid, epoch: 0 }
}

/// The parameter sets of a compressed sample's format description, in the order CoreMedia lists
/// them.
fn parameter_sets(sample: &CMSampleBuffer, codec: Codec) -> Result<Vec<Vec<u8>>, String> {
    // SAFETY: `sample` is a valid sample buffer for the duration of the callback.
    let format = unsafe { sample.format_description() }.ok_or("a compressed frame without a format description")?;
    let mut sets = Vec::new();
    let (mut index, mut count) = (0usize, 1usize);
    while index < count {
        if index >= MAX_SETS {
            return Err(format!("the format description lists more than {MAX_SETS} parameter sets"));
        }
        let (mut ptr, mut size, mut total, mut nal_len): (*const u8, usize, usize, c_int) = (std::ptr::null(), 0, 0, 0);
        // SAFETY: the out-pointers are valid for the call, and the returned pointer is read only
        // while `format`, which owns the bytes, is alive.
        let status = unsafe {
            match codec {
                Codec::H264 => CMVideoFormatDescriptionGetH264ParameterSetAtIndex(&format, index, &mut ptr, &mut size, &mut total, &mut nal_len),
                Codec::Hevc => CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(&format, index, &mut ptr, &mut size, &mut total, &mut nal_len),
            }
        };
        if status != 0 {
            return Err(format!("the parameter sets could not be read ({status})"));
        }
        if nal_len != 4 {
            return Err(format!("the samples carry {nal_len}-byte NAL unit lengths, not 4-byte ones"));
        }
        if ptr.is_null() || size == 0 || size > 65_535 {
            return Err(format!("a parameter set of {size} bytes"));
        }
        count = total;
        // SAFETY: CoreMedia returned `size` readable bytes at `ptr`, owned by `format`.
        sets.push(unsafe { std::slice::from_raw_parts(ptr, size) }.to_vec());
        index += 1;
    }
    Ok(sets)
}

/// The bytes, time and parameter sets of one compressed sample.
fn read_output(sample: &CMSampleBuffer, shared: &Shared) -> Result<Output, String> {
    // SAFETY: `sample` is a valid sample buffer for the duration of the callback.
    let block = unsafe { sample.data_buffer() }.ok_or("a compressed frame without data")?;
    // SAFETY: `block` is a valid block buffer, retained above.
    let len = unsafe { block.data_length() };
    if len == 0 || len > MAX_FRAME_BYTES {
        return Err(format!("a compressed frame of {len} bytes"));
    }
    let mut data = vec![0u8; len];
    let dest = NonNull::new(data.as_mut_ptr().cast::<c_void>()).ok_or("an empty frame buffer")?;
    // SAFETY: `dest` points to `len` writable bytes, the block's own length.
    let status = unsafe { block.copy_data_bytes(0, len, dest) };
    if status != 0 {
        return Err(format!("a compressed frame could not be copied ({status})"));
    }
    // SAFETY: valid sample buffer, see above.
    let pts = unsafe { sample.presentation_time_stamp() };
    let time = if pts.flags.contains(CMTimeFlags::Valid) { rescale(pts.value, pts.timescale, shared.timescale) } else { None };
    let sets = parameter_sets(sample, shared.codec)?;
    Ok(Output { data, time, sets })
}

/// The output callback. A panic inside is caught and reported as a failed frame, so none unwinds
/// into VideoToolbox.
unsafe extern "C-unwind" fn output_callback(
    refcon: *mut c_void,
    _frame: *mut c_void,
    status: i32,
    flags: VTEncodeInfoFlags,
    sample: *mut CMSampleBuffer,
) {
    if refcon.is_null() {
        return;
    }
    // SAFETY: `refcon` is the `Shared` the session was created with. It lives in a `Box` owned by
    // the `Session`, which invalidates the session, so no callback runs, before freeing it.
    let shared = unsafe { &*(refcon as *const Shared) };
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if status != 0 {
            return Err(format!("VideoToolbox could not encode a frame ({status})"));
        }
        if flags.contains(VTEncodeInfoFlags::FrameDropped) {
            return Err("VideoToolbox dropped a frame".into());
        }
        let Some(sample) = NonNull::new(sample) else { return Err("VideoToolbox returned no data for a frame".into()) };
        // SAFETY: VideoToolbox passes a valid sample buffer that stays alive for the duration of the
        // callback; it is only borrowed here.
        read_output(unsafe { sample.as_ref() }, shared)
    }));
    shared.push(result.unwrap_or_else(|_| Err("a compressed frame could not be read".into())));
}

unsafe extern "C" {
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

/// `kVTCompressionPropertyKey_ConstantBitRate`, or `None` on a macOS without it. The key exists
/// from macOS 13. Linking it by name would stop the app from starting on macOS 11 and 12, so it is
/// looked up at run time.
fn constant_bitrate_key() -> Option<&'static CFString> {
    // `RTLD_DEFAULT` on macOS: every image loaded into the process.
    const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;
    const NAME: &CStr = c"kVTCompressionPropertyKey_ConstantBitRate";
    // SAFETY: `NAME` is a NUL-terminated string that `dlsym` only reads.
    let symbol = unsafe { dlsym(RTLD_DEFAULT, NAME.as_ptr()) };
    let slot = NonNull::new(symbol)?.cast::<*const CFString>();
    // SAFETY: the symbol is a `CFStringRef` constant VideoToolbox exports, so `slot` points at a
    // pointer to an immutable CFString that lives as long as the framework stays loaded, which is
    // the whole process. It is read once and never written.
    unsafe { slot.read().as_ref() }
}

fn set_value(session: &VTCompressionSession, key: &CFString, value: &CFType) -> i32 {
    // SAFETY: the session is valid, `key` is a framework constant and `value` a live CF object.
    unsafe { VTSessionSetProperty(session.as_ref(), key, Some(value)) }
}

fn set_bool(session: &VTCompressionSession, key: &CFString, on: bool) -> i32 {
    set_value(session, key, CFBoolean::new(on).as_ref())
}

fn set_i32(session: &VTCompressionSession, key: &CFString, v: i32) -> i32 {
    set_value(session, key, CFNumber::new_i32(v).as_ref())
}

/// An error naming the property VideoToolbox refused.
fn require(status: i32, what: &str) -> Result<(), OpenError> {
    if status == 0 { Ok(()) } else { Err(OpenError::Unsupported(format!("VideoToolbox refused {what} ({status})"))) }
}

/// Set an average bitrate of `bps`. With `ceiling`, the session also gets a one-second ceiling at
/// the same rate, and an encoder that refuses the ceiling is an error.
fn average_bitrate(session: &VTCompressionSession, bps: i32, ceiling: bool) -> Result<(), OpenError> {
    // SAFETY: reading immutable framework constants.
    let (avg_key, limits_key) = unsafe { (kVTCompressionPropertyKey_AverageBitRate, kVTCompressionPropertyKey_DataRateLimits) };
    require(set_i32(session, avg_key, bps), "the bitrate")?;
    if ceiling {
        let limits = CFArray::<CFType>::from_objects(&[CFNumber::new_i32(bps / 8).as_ref(), CFNumber::new_f64(1.0).as_ref()]);
        require(set_value(session, limits_key, limits.as_ref()), "the bitrate ceiling")?;
    }
    Ok(())
}

fn configure(session: &VTCompressionSession, p: &Params) -> Result<(), OpenError> {
    // SAFETY: reading immutable framework constants.
    let (profile_key, profile) = unsafe {
        (
            kVTCompressionPropertyKey_ProfileLevel,
            match p.codec {
                Codec::H264 => kVTProfileLevel_H264_High_AutoLevel,
                Codec::Hevc => kVTProfileLevel_HEVC_Main_AutoLevel,
            },
        )
    };
    require(set_value(session, profile_key, profile.as_ref()), "the profile")?;
    // SAFETY: reading immutable framework constants.
    let (realtime, reorder, open_gop) =
        unsafe { (kVTCompressionPropertyKey_RealTime, kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AllowOpenGOP) };
    let _ = set_bool(session, realtime, true);
    require(set_bool(session, reorder, false), "turning off frame reordering")?;
    let _ = set_bool(session, open_gop, false);
    // SAFETY: reading immutable framework constants.
    let (keyint_key, fps_key) = unsafe { (kVTCompressionPropertyKey_MaxKeyFrameInterval, kVTCompressionPropertyKey_ExpectedFrameRate) };
    require(set_i32(session, keyint_key, i32::try_from(p.gop).unwrap_or(i32::MAX)), "the keyframe interval")?;
    let _ = set_value(session, fps_key, CFNumber::new_f64(f64::from(p.fps.0) / f64::from(p.fps.1.max(1))).as_ref());
    match p.rate {
        Rate::Quality(q) => {
            // SAFETY: reading an immutable framework constant.
            let key = unsafe { kVTCompressionPropertyKey_Quality };
            // An encoder without a quality setting gets an average bitrate instead.
            if set_value(session, key, CFNumber::new_f64(quality(q)).as_ref()) != 0 {
                average_bitrate(session, nominal_bitrate(p), false)?;
            }
        }
        Rate::Cbr(kbps) => {
            let bps = bitrate_bps(kbps);
            if constant_bitrate_key().is_none_or(|key| set_i32(session, key, bps) != 0) {
                average_bitrate(session, bps, true)?;
            }
        }
    }
    // SAFETY: reading immutable framework constants.
    let (pk, tk, mk, pv, tv, mv): (&CFString, &CFString, &CFString, &CFString, &CFString, &CFString) = unsafe {
        (
            kVTCompressionPropertyKey_ColorPrimaries,
            kVTCompressionPropertyKey_TransferFunction,
            kVTCompressionPropertyKey_YCbCrMatrix,
            kCVImageBufferColorPrimaries_ITU_R_709_2,
            kCVImageBufferTransferFunction_ITU_R_709_2,
            kCVImageBufferYCbCrMatrix_ITU_R_709_2,
        )
    };
    let _ = set_value(session, pk, pv.as_ref());
    let _ = set_value(session, tk, tv.as_ref());
    let _ = set_value(session, mk, mv.as_ref());
    Ok(())
}

/// Unlocks a pixel buffer's base address when dropped.
struct Locked<'a>(&'a CVImageBuffer);

impl Drop for Locked<'_> {
    fn drop(&mut self) {
        // SAFETY: `fill` locked the buffer with the same flags, and the buffer is still alive.
        unsafe { CVPixelBufferUnlockBaseAddress(self.0, CVPixelBufferLockFlags(0)) };
    }
}

/// One writable plane of a locked pixel buffer, with its row stride, width and height.
fn plane_mut<'l>(pb: &CVImageBuffer, _lock: &'l mut Locked<'_>, i: usize) -> Result<(&'l mut [u8], usize, usize, usize), String> {
    let base = CVPixelBufferGetBaseAddressOfPlane(pb, i).cast::<u8>();
    let stride = CVPixelBufferGetBytesPerRowOfPlane(pb, i);
    let (width, height) = (CVPixelBufferGetWidthOfPlane(pb, i), CVPixelBufferGetHeightOfPlane(pb, i));
    if base.is_null() || stride == 0 || height == 0 {
        return Err(format!("plane {i} of the input buffer is not mapped"));
    }
    let len = stride.checked_mul(height).ok_or("the input buffer's plane size overflows")?;
    // SAFETY: the base address is locked for writing while `_lock` lives, and the slice borrows it
    // for 'l; CoreVideo maps `bytes_per_row × height` bytes from each plane's base.
    Ok((unsafe { std::slice::from_raw_parts_mut(base, len) }, stride, width, height))
}

/// Copy an NV12 frame of `w` × `h` (the luma rows, then the interleaved chroma rows) into `pb`.
fn fill(pb: &CVImageBuffer, w: usize, h: usize, nv12: &[u8]) -> Result<(), String> {
    let (luma_src, chroma_src) = nv12.split_at_checked(w * h).ok_or("the frame is smaller than the picture")?;
    // SAFETY: `pb` is a valid pixel buffer the caller holds retained.
    if unsafe { CVPixelBufferLockBaseAddress(pb, CVPixelBufferLockFlags(0)) } != 0 {
        return Err("the input buffer could not be locked".into());
    }
    let mut lock = Locked(pb);
    let (luma, ls, lw, lh) = plane_mut(pb, &mut lock, 0)?;
    if lw < w || lh < h || ls < w {
        return Err("the input buffer's luma plane is smaller than the picture".into());
    }
    for (row, src) in luma.chunks_mut(ls).zip(luma_src.chunks_exact(w)).take(h) {
        row.get_mut(..w).ok_or("a short luma row")?.copy_from_slice(src);
    }
    let (chroma, cs, cw, ch) = plane_mut(pb, &mut lock, 1)?;
    if cw * 2 < w || ch * 2 < h || cs < w {
        return Err("the input buffer's chroma plane is smaller than the picture".into());
    }
    for (row, src) in chroma.chunks_mut(cs).zip(chroma_src.chunks_exact(w)).take(h / 2) {
        row.get_mut(..w).ok_or("a short chroma row")?.copy_from_slice(src);
    }
    drop(lock);
    Ok(())
}

impl Session {
    /// Create a session for `p` with a hardware encoder required, or say why none opens.
    pub(crate) fn open(p: &Params) -> Result<Session, OpenError> {
        let (w, h) = (i32::try_from(p.width).unwrap_or(i32::MAX), i32::try_from(p.height).unwrap_or(i32::MAX));
        let timescale = i32::try_from(p.fps.0).map_err(|_| OpenError::Unsupported(format!("the frame rate {}/{}", p.fps.0, p.fps.1)))?;
        let shared = Box::new(Shared { codec: p.codec, timescale, out: Mutex::new(Vec::new()) });
        // SAFETY: reading an immutable framework constant.
        let require_key: &CFString = unsafe { kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder };
        let spec = CFDictionary::<CFString, CFType>::from_slices(&[require_key], &[CFBoolean::new(true).as_ref()]);
        let (pf, cw, ch) = (CFNumber::new_i32(NV12_VIDEO as i32), CFNumber::new_i32(w), CFNumber::new_i32(h));
        let io = CFDictionary::<CFString, CFType>::from_slices(&[], &[]);
        // SAFETY: reading immutable framework constants.
        let (pf_key, w_key, h_key, io_key): (&CFString, &CFString, &CFString, &CFString) =
            unsafe { (kCVPixelBufferPixelFormatTypeKey, kCVPixelBufferWidthKey, kCVPixelBufferHeightKey, kCVPixelBufferIOSurfacePropertiesKey) };
        let attrs =
            CFDictionary::<CFString, CFType>::from_slices(&[pf_key, w_key, h_key, io_key], &[pf.as_ref(), cw.as_ref(), ch.as_ref(), io.as_ref()]);
        let codec = match p.codec {
            Codec::H264 => kCMVideoCodecType_H264,
            Codec::Hevc => kCMVideoCodecType_HEVC,
        };
        let mut out: *mut VTCompressionSession = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call, and VideoToolbox copies what it keeps. The
        // callback's refcon points into `shared`, which the returned `Session` keeps alive until the
        // session is invalidated.
        let status = unsafe {
            VTCompressionSession::create(
                None,
                w,
                h,
                codec,
                Some(spec.as_ref()),
                Some(attrs.as_ref()),
                None,
                Some(output_callback),
                (&raw const *shared).cast_mut().cast::<c_void>(),
                NonNull::from(&mut out),
            )
        };
        let Some(raw) = NonNull::new(out).filter(|_| status == 0) else {
            return Err(open_error(status, format!("no hardware encoder opened ({status})"), OpenError::NoDevice));
        };
        // SAFETY: a created session is returned retained (+1), and `CFRetained` takes that
        // reference over.
        let session = unsafe { CFRetained::from_raw(raw) };
        let mut s = Session { session, shared, pool: None, attrs, width: p.width as usize, height: p.height as usize, timescale };
        configure(&s.session, p)?;
        // SAFETY: the session is valid. Preparing allocates the encoder's resources now, so a
        // stream the hardware cannot take fails here and not on the first frame.
        let status = unsafe { s.session.prepare_to_encode_frames() };
        if status != 0 {
            return Err(open_error(status, format!("VideoToolbox refused the stream ({status})"), OpenError::Unsupported));
        }
        if !s.uses_hardware() {
            return Err(OpenError::NoDevice("VideoToolbox opened a software encoder".into()));
        }
        // SAFETY: the session is valid; the pool, when there is one, is returned retained.
        s.pool = unsafe { s.session.pixel_buffer_pool() };
        Ok(s)
    }

    /// Whether VideoToolbox says the session runs on a hardware encoder.
    fn uses_hardware(&self) -> bool {
        // SAFETY: reading an immutable framework constant.
        let key: &CFString = unsafe { kVTCompressionPropertyKey_UsingHardwareAcceleratedVideoEncoder };
        let mut value: *mut CFType = std::ptr::null_mut();
        // SAFETY: the session is valid and `value` is a valid out-pointer for the copied property,
        // which comes back retained (+1); it stays null when the call fails.
        let status = unsafe { VTSessionCopyProperty(self.session.as_ref(), key, None, (&raw mut value).cast()) };
        let Some(value) = NonNull::new(value).filter(|_| status == 0) else { return false };
        // SAFETY: a copied property is returned retained (+1), and `CFRetained` takes it over.
        let value = unsafe { CFRetained::from_raw(value) };
        value.downcast_ref::<CFBoolean>().is_some_and(CFBoolean::as_bool)
    }

    fn new_buffer(&self) -> Result<CFRetained<CVImageBuffer>, String> {
        let mut raw: *mut CVImageBuffer = std::ptr::null_mut();
        let status = match &self.pool {
            // SAFETY: the pool is valid and `raw` is a valid out-pointer.
            Some(pool) => unsafe { CVPixelBufferPool::create_pixel_buffer(None, pool, NonNull::from(&mut raw)) },
            // SAFETY: `attrs` is a valid dictionary of CF types and `raw` a valid out-pointer.
            None => unsafe { CVPixelBufferCreate(None, self.width, self.height, NV12_VIDEO, Some(self.attrs.as_ref()), NonNull::from(&mut raw)) },
        };
        let raw = NonNull::new(raw).filter(|_| status == 0).ok_or_else(|| format!("no input buffer could be allocated ({status})"))?;
        // SAFETY: both functions return the buffer retained (+1).
        Ok(unsafe { CFRetained::from_raw(raw) })
    }

    /// Send the NV12 `frame` at `time` lasting `duration` (both in units of 1 / the frame rate's
    /// numerator), and move the samples that came out to `out`.
    pub(crate) fn encode(&mut self, frame: &[u8], time: i64, duration: i64, out: &mut Vec<Output>) -> Result<(), String> {
        let buffer = self.new_buffer()?;
        fill(&buffer, self.width, self.height, frame)?;
        let (pts, duration) = (cm_time(time, self.timescale), cm_time(duration, self.timescale));
        // SAFETY: the session and buffer are valid, and VideoToolbox retains the buffer as long as
        // it needs it; no frame properties, refcon or info-flags out-pointer are passed.
        let status = unsafe { self.session.encode_frame(&buffer, pts, duration, None, std::ptr::null_mut(), std::ptr::null_mut()) };
        if status != 0 {
            return Err(format!("VideoToolbox did not take a frame ({status})"));
        }
        self.take(out)
    }

    /// Finish every frame in flight and move its sample to `out`.
    pub(crate) fn drain(&mut self, out: &mut Vec<Output>) -> Result<(), String> {
        // SAFETY: the session is valid; an invalid time means every frame in flight.
        let status = unsafe { self.session.complete_frames(invalid_time()) };
        if status != 0 {
            return Err(format!("VideoToolbox could not finish the frames ({status})"));
        }
        self.take(out)
    }

    fn take(&mut self, out: &mut Vec<Output>) -> Result<(), String> {
        let done = std::mem::take(&mut *self.shared.out.lock().unwrap_or_else(PoisonError::into_inner));
        for o in done {
            out.push(o?);
        }
        Ok(())
    }
}
