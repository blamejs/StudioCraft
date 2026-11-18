//! libva on 64-bit Linux: `libva.so.2` and `libva-drm.so.2` are loaded at run time (a system
//! without them has no VA-API encoder, never a failure to start), a display is opened on the first
//! DRM render node whose driver encodes the codec, and a [`Session`] holds one stream's
//! configuration, context, surfaces and coded buffers.
//!
//! - [`DEPTH`] frames can be in flight. Each has its own input surface and coded buffer, so a frame
//!   is copied in and submitted while the GPU still encodes the ones before it, and only reading the
//!   oldest frame's coded buffer waits for the GPU.
//! - Frames are copied into the surface through `vaDeriveImage` (the surface's own memory). A
//!   driver that cannot map its surfaces gets one image made at the start, copied into the surface
//!   with `vaPutImage`.
//! - Every unsafe block has a SAFETY comment; the one callback into Rust (libva's error messages)
//!   catches panics.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::sync::OnceLock;

use libloading::Library;

use super::abi::{self, VABufferID, VAConfigAttrib, VAImage, VAImageFormat, VASurfaceID};
use super::{
    Blocks, Control, DEPTH, OpenError, Params, Picture, RECON, coded_bytes, coded_size, control, copy_nv12, frame_rate_buffer, h264_picture,
    h264_sequence, h264_slice, hevc_blocks, hevc_picture, hevc_sequence, hevc_slice, nominal_bitrate, open_error, rate_buffers, recon_slots,
    segment_error, telling,
};
use crate::Codec;

/// The most entry points a driver may report for one profile; the entry points of libva 2.22.0 run
/// from 1 to 14.
const MAX_ENTRYPOINTS: usize = 256;

/// The libva entry points (the libraries stay loaded for the life of the process).
struct Api {
    _va: Library,
    _drm: Library,
    initialize: abi::vaInitialize,
    terminate: abi::vaTerminate,
    error_str: abi::vaErrorStr,
    vendor_string: abi::vaQueryVendorString,
    set_info_callback: abi::vaSetInfoCallback,
    set_error_callback: abi::vaSetErrorCallback,
    max_num_entrypoints: abi::vaMaxNumEntrypoints,
    query_config_entrypoints: abi::vaQueryConfigEntrypoints,
    get_config_attributes: abi::vaGetConfigAttributes,
    create_config: abi::vaCreateConfig,
    destroy_config: abi::vaDestroyConfig,
    create_surfaces: abi::vaCreateSurfaces,
    destroy_surfaces: abi::vaDestroySurfaces,
    create_context: abi::vaCreateContext,
    destroy_context: abi::vaDestroyContext,
    create_buffer: abi::vaCreateBuffer,
    destroy_buffer: abi::vaDestroyBuffer,
    begin_picture: abi::vaBeginPicture,
    render_picture: abi::vaRenderPicture,
    end_picture: abi::vaEndPicture,
    sync_surface: abi::vaSyncSurface,
    create_image: abi::vaCreateImage,
    derive_image: abi::vaDeriveImage,
    destroy_image: abi::vaDestroyImage,
    put_image: abi::vaPutImage,
    map_buffer: abi::vaMapBuffer,
    unmap_buffer: abi::vaUnmapBuffer,
    get_display_drm: abi::vaGetDisplayDRM,
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

fn api() -> Result<&'static Api, String> {
    API.get_or_init(load).as_ref().map_err(Clone::clone)
}

/// Whether libva and its DRM display library load. They are loaded once per process, so later
/// calls cost nothing.
pub(crate) fn driver() -> Result<(), String> {
    api().map(drop)
}

fn load() -> Result<Api, String> {
    // SAFETY: loading libva runs its library initializers, which only set up libva's own state;
    // it is the system's VA-API runtime, loaded the way every VA-API client loads it.
    let va = unsafe { Library::new("libva.so.2") }.map_err(|e| format!("libva.so.2: {e}"))?;
    // SAFETY: as above, for libva's DRM display library.
    let drm = unsafe { Library::new("libva-drm.so.2") }.map_err(|e| format!("libva-drm.so.2: {e}"))?;
    macro_rules! sym {
        ($lib:expr, $name:ident) => {{
            // SAFETY: `abi::$name` is the C signature of the libva function of that name,
            // transcribed from va.h. The pointer is copied out; the library it points into is kept
            // loaded in `Api` for the life of the process.
            let s = unsafe { $lib.get::<abi::$name>(concat!(stringify!($name), "\0").as_bytes()) };
            *s.map_err(|e| format!("{}: {e}", stringify!($name)))?
        }};
    }
    Ok(Api {
        initialize: sym!(va, vaInitialize),
        terminate: sym!(va, vaTerminate),
        error_str: sym!(va, vaErrorStr),
        vendor_string: sym!(va, vaQueryVendorString),
        set_info_callback: sym!(va, vaSetInfoCallback),
        set_error_callback: sym!(va, vaSetErrorCallback),
        max_num_entrypoints: sym!(va, vaMaxNumEntrypoints),
        query_config_entrypoints: sym!(va, vaQueryConfigEntrypoints),
        get_config_attributes: sym!(va, vaGetConfigAttributes),
        create_config: sym!(va, vaCreateConfig),
        destroy_config: sym!(va, vaDestroyConfig),
        create_surfaces: sym!(va, vaCreateSurfaces),
        destroy_surfaces: sym!(va, vaDestroySurfaces),
        create_context: sym!(va, vaCreateContext),
        destroy_context: sym!(va, vaDestroyContext),
        create_buffer: sym!(va, vaCreateBuffer),
        destroy_buffer: sym!(va, vaDestroyBuffer),
        begin_picture: sym!(va, vaBeginPicture),
        render_picture: sym!(va, vaRenderPicture),
        end_picture: sym!(va, vaEndPicture),
        sync_surface: sym!(va, vaSyncSurface),
        create_image: sym!(va, vaCreateImage),
        derive_image: sym!(va, vaDeriveImage),
        destroy_image: sym!(va, vaDestroyImage),
        put_image: sym!(va, vaPutImage),
        map_buffer: sym!(va, vaMapBuffer),
        unmap_buffer: sym!(va, vaUnmapBuffer),
        get_display_drm: sym!(drm, vaGetDisplayDRM),
        _va: va,
        _drm: drm,
    })
}

/// libva's error messages, into the log instead of stderr.
extern "C" fn on_error(_user: *mut c_void, message: *const c_char) {
    let _ = std::panic::catch_unwind(|| {
        if message.is_null() {
            return;
        }
        // SAFETY: libva passes a NUL-terminated message that is valid for the duration of the call.
        let text = unsafe { CStr::from_ptr(message) }.to_string_lossy();
        log::warn!("libva: {}", text.trim_end());
    });
}

/// The `/dev/dri/renderD*` nodes in number order.
fn render_nodes() -> Vec<String> {
    let mut nodes: Vec<(u32, String)> = std::fs::read_dir("/dev/dri")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let n = name.strip_prefix("renderD")?.parse().ok()?;
            Some((n, format!("/dev/dri/{name}")))
        })
        .collect();
    nodes.sort();
    nodes.into_iter().map(|(_, path)| path).collect()
}

/// An initialized VA display on a DRM render node.
struct Display {
    api: &'static Api,
    dpy: abi::VADisplay,
    /// The render node and the driver's description, for messages.
    name: String,
    /// The render node; open for as long as the display uses it.
    _device: File,
}

impl Display {
    fn open(api: &'static Api, path: &str) -> Result<Display, String> {
        let device = OpenOptions::new().read(true).write(true).open(path).map_err(|e| format!("{path}: {e}"))?;
        // SAFETY: the descriptor is an open render node, kept open by `device` (moved into the
        // returned display) for as long as the display exists.
        let dpy = unsafe { (api.get_display_drm)(device.as_raw_fd()) };
        if dpy.is_null() {
            return Err(format!("{path}: vaGetDisplayDRM failed"));
        }
        // From here on `Drop` terminates the display.
        let mut d = Display { api, dpy, name: path.to_string(), _device: device };
        // SAFETY: `dpy` is a display from vaGetDisplayDRM; a null info callback turns libva's info
        // messages off, and `on_error` matches `VAMessageCallback` and never unwinds.
        unsafe {
            (api.set_info_callback)(dpy, None, std::ptr::null_mut());
            (api.set_error_callback)(dpy, Some(on_error), std::ptr::null_mut());
        }
        let (mut major, mut minor): (c_int, c_int) = (0, 0);
        // SAFETY: `dpy` is valid; the version outputs point to live locals.
        let st = unsafe { (api.initialize)(dpy, &mut major, &mut minor) };
        d.check(st, "vaInitialize").map_err(|e| format!("{path}: {e}"))?;
        // SAFETY: `dpy` is an initialized display; the vendor string is NUL-terminated and owned by
        // the driver for the life of the display (copied out here).
        let vendor = unsafe { (api.vendor_string)(dpy) };
        if !vendor.is_null() {
            // SAFETY: non-null, NUL-terminated (see above).
            let vendor = unsafe { CStr::from_ptr(vendor) }.to_string_lossy();
            d.name = format!("{path} ({})", vendor.trim());
        }
        Ok(d)
    }

    /// `Ok` for a successful status, else an error naming `what` and libva's text for the status.
    fn check(&self, st: abi::VAStatus, what: &str) -> Result<(), String> {
        if st == abi::VA_STATUS_SUCCESS {
            return Ok(());
        }
        // SAFETY: vaErrorStr takes any status and returns a static NUL-terminated string.
        let text = unsafe { (self.api.error_str)(st) };
        let text = if text.is_null() {
            String::new()
        } else {
            // SAFETY: non-null static NUL-terminated string (see above).
            unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned()
        };
        Err(format!("{what}: {text} ({st:#x})"))
    }

    /// The entry points the driver offers for `profile`; none when it does not know the profile.
    fn entrypoints(&self, profile: abi::VAProfile) -> Vec<abi::VAEntrypoint> {
        // SAFETY: `dpy` is an initialized display.
        let max = unsafe { (self.api.max_num_entrypoints)(self.dpy) };
        // A driver that reports no entry points, or more than any driver has, is not asked.
        let Some(max) = usize::try_from(max).ok().filter(|m| (1..=MAX_ENTRYPOINTS).contains(m)) else { return Vec::new() };
        let mut list = vec![0; max];
        let mut n: c_int = 0;
        // SAFETY: `list` has room for the `vaMaxNumEntrypoints` entries the call may write, and `n`
        // is a live output.
        let st = unsafe { (self.api.query_config_entrypoints)(self.dpy, profile, list.as_mut_ptr(), &mut n) };
        if st != abi::VA_STATUS_SUCCESS {
            return Vec::new();
        }
        list.truncate(usize::try_from(n).unwrap_or(0).min(max));
        list
    }

    /// The driver's values of the attributes in `attrs` for `profile` and `entry`.
    fn attributes(&self, profile: abi::VAProfile, entry: abi::VAEntrypoint, attrs: &mut [VAConfigAttrib]) -> Result<(), String> {
        let n = c_int::try_from(attrs.len()).map_err(|_| "too many attributes")?;
        // SAFETY: `attrs` holds `n` live attributes that the driver fills in.
        let st = unsafe { (self.api.get_config_attributes)(self.dpy, profile, entry, attrs.as_mut_ptr(), n) };
        self.check(st, "vaGetConfigAttributes")
    }

    /// The first profile and encode entry point of the driver that takes `p`, with its rate
    /// control and block sizes. A driver without any encode entry point for the codec is
    /// `NoDevice`; one whose encoders all refuse `p` is `Unsupported`.
    fn choose(&self, p: &Params) -> Result<Choice, OpenError> {
        let profiles: &[abi::VAProfile] = match p.codec {
            Codec::H264 => &[abi::VAProfileH264High, abi::VAProfileH264Main, abi::VAProfileH264ConstrainedBaseline],
            Codec::Hevc => &[abi::VAProfileHEVCMain],
        };
        let mut why = None;
        for &profile in profiles {
            let entries = self.entrypoints(profile);
            for entry in [abi::VAEntrypointEncSlice, abi::VAEntrypointEncSliceLP] {
                if !entries.contains(&entry) {
                    continue;
                }
                match self.check_entry(p, profile, entry) {
                    Ok(choice) => return Ok(choice),
                    Err(e) => why = Some(e),
                }
            }
        }
        let codec = if p.codec == Codec::H264 { "H.264" } else { "HEVC" };
        Err(match why {
            Some(why) => OpenError::Unsupported(format!("{}: {why}", self.name)),
            None => OpenError::NoDevice(format!("{} has no {codec} encoder", self.name)),
        })
    }

    fn check_entry(&self, p: &Params, profile: abi::VAProfile, entry: abi::VAEntrypoint) -> Result<Choice, String> {
        let mut attrs = [
            abi::VAConfigAttribRTFormat,
            abi::VAConfigAttribRateControl,
            abi::VAConfigAttribMaxPictureWidth,
            abi::VAConfigAttribMaxPictureHeight,
            abi::VAConfigAttribEncHEVCBlockSizes,
        ]
        .map(|type_| VAConfigAttrib { type_, value: 0 });
        self.attributes(profile, entry, &mut attrs)?;
        let [rt, rc, max_w, max_h, blocks] = attrs.map(|a| a.value);
        if rt == abi::VA_ATTRIB_NOT_SUPPORTED || rt & abi::VA_RT_FORMAT_YUV420 == 0 {
            return Err("the encoder takes no 8-bit 4:2:0 pictures".into());
        }
        let (w, h) = (p.width, p.height);
        let over = |max: u32, side: u32| max != abi::VA_ATTRIB_NOT_SUPPORTED && max != 0 && side > max;
        if over(max_w, w) || over(max_h, h) {
            return Err(format!("{w}x{h}: the encoder takes at most {max_w}x{max_h}"));
        }
        let blocks = hevc_blocks(blocks);
        let min = 1u32 << blocks.min_cb;
        if p.codec == Codec::Hevc && (w % min != 0 || h % min != 0) {
            return Err(format!("{w}x{h}: the HEVC encoder takes widths and heights that are multiples of {min}"));
        }
        let ctl = control(p.rate, rc, nominal_bitrate(p))?;
        Ok(Choice { profile, entry, ctl, blocks })
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // SAFETY: `dpy` came from vaGetDisplayDRM and is terminated exactly once, here, after every
        // object made on it was destroyed (`Session::drop` runs before its fields drop).
        unsafe { (self.api.terminate)(self.dpy) };
    }
}

/// The profile, entry point and settings a session encodes with.
#[derive(Clone, Copy, Debug)]
struct Choice {
    profile: abi::VAProfile,
    entry: abi::VAEntrypoint,
    ctl: Control,
    blocks: Blocks,
}

/// How frames get into the input surfaces.
#[derive(Clone, Copy, Debug)]
enum Upload {
    /// Not tried yet.
    Unknown,
    /// Mapped through `vaDeriveImage`.
    Derive,
    /// Copied into this image and from it into the surface with `vaPutImage`.
    Put(VAImage),
}

/// One stream's encoder on a VA display.
pub(crate) struct Session {
    display: Display,
    config: abi::VAConfigID,
    context: abi::VAContextID,
    /// [`DEPTH`] input surfaces, then [`RECON`] reconstructed ones.
    surfaces: Vec<VASurfaceID>,
    /// One coded buffer per input surface.
    coded: Vec<VABufferID>,
    choice: Choice,
    p: Params,
    /// Macroblocks (H.264) or coding tree blocks (HEVC) in a picture.
    units: u32,
    upload: Upload,
    /// The slot of the oldest frame in flight, and how many are in flight.
    head: usize,
    count: usize,
    /// Pictures submitted so far.
    frames: u64,
}

// SAFETY: a VA display and its objects are not tied to the thread that made them (va.h,
// "Multithreading Guide"). Every object here belongs to this session alone and is used only
// through `&mut self` or `&self` of its one owner, so one thread uses them at a time.
unsafe impl Send for Session {}

impl Session {
    /// Open an encoder for `p` on the first render node whose driver encodes the codec.
    pub(crate) fn open(p: &Params) -> Result<Session, OpenError> {
        let api = api().map_err(OpenError::NoDevice)?;
        let mut why: Option<OpenError> = None;
        for node in render_nodes() {
            let found = Display::open(api, &node).map_err(OpenError::NoDevice).and_then(|d| d.choose(p).map(|c| (d, c)));
            let e = match found.and_then(|(display, choice)| Session::on(display, choice, p)) {
                Ok(s) => return Ok(s),
                Err(e) => e,
            };
            why = Some(match why {
                Some(kept) => telling(kept, e),
                None => e,
            });
        }
        Err(why.unwrap_or_else(|| OpenError::NoDevice("no DRM render node (/dev/dri/renderD*)".into())))
    }

    fn on(display: Display, choice: Choice, p: &Params) -> Result<Session, OpenError> {
        let (api, dpy) = (display.api, display.dpy);
        let name = display.name.clone();
        let fail = |st: abi::VAStatus, e: String| open_error(st, format!("{name}: {e}"), OpenError::Unsupported);
        let mut attrs = [
            VAConfigAttrib { type_: abi::VAConfigAttribRTFormat, value: abi::VA_RT_FORMAT_YUV420 },
            VAConfigAttrib { type_: abi::VAConfigAttribRateControl, value: choice.ctl.mode() },
        ];
        let mut config = abi::VA_INVALID_ID;
        // SAFETY: `dpy` is initialized; `attrs` holds 2 live attributes and `config` is a live output.
        let st = unsafe { (api.create_config)(dpy, choice.profile, choice.entry, attrs.as_mut_ptr(), 2, &mut config) };
        display.check(st, "vaCreateConfig").map_err(|e| fail(st, e))?;
        let (cw, ch) = coded_size(p.codec, p.width, p.height);
        let units = match p.codec {
            Codec::H264 => (cw / 16) * (ch / 16),
            Codec::Hevc => p.width.div_ceil(1 << choice.blocks.ctb) * p.height.div_ceil(1 << choice.blocks.ctb),
        };
        // From here on `Drop` releases what was made.
        let mut s = Session {
            display,
            config,
            context: abi::VA_INVALID_ID,
            surfaces: Vec::new(),
            coded: Vec::new(),
            choice,
            p: *p,
            units,
            upload: Upload::Unknown,
            head: 0,
            count: 0,
            frames: 0,
        };
        let mut surfaces = vec![abi::VA_INVALID_SURFACE; DEPTH + RECON];
        // SAFETY: `surfaces` has room for DEPTH + RECON ids; no attributes are passed (null, 0).
        let st = unsafe {
            (api.create_surfaces)(dpy, abi::VA_RT_FORMAT_YUV420, cw, ch, surfaces.as_mut_ptr(), (DEPTH + RECON) as c_uint, std::ptr::null_mut(), 0)
        };
        s.display.check(st, "vaCreateSurfaces").map_err(|e| fail(st, e))?;
        s.surfaces = surfaces;
        let (w, h) = (c_int::try_from(cw).unwrap_or(c_int::MAX), c_int::try_from(ch).unwrap_or(c_int::MAX));
        let mut context = abi::VA_INVALID_ID;
        // SAFETY: `config` and the DEPTH + RECON surfaces were made on this display; `context` is a
        // live output.
        let st = unsafe {
            (api.create_context)(dpy, s.config, w, h, abi::VA_PROGRESSIVE, s.surfaces.as_mut_ptr(), (DEPTH + RECON) as c_int, &mut context)
        };
        s.display.check(st, "vaCreateContext").map_err(|e| fail(st, e))?;
        s.context = context;
        for _ in 0..DEPTH {
            let mut id = abi::VA_INVALID_ID;
            // SAFETY: a coded buffer of `coded_bytes` is made on this context with no initial data
            // (null); `id` is a live output.
            let st = unsafe { (api.create_buffer)(dpy, context, abi::VAEncCodedBufferType, coded_bytes(cw, ch), 1, std::ptr::null_mut(), &mut id) };
            s.display.check(st, "vaCreateBuffer").map_err(|e| fail(st, e))?;
            s.coded.push(id);
        }
        Ok(s)
    }

    /// Whether every frame slot holds a frame whose coded data has not been read.
    pub(crate) fn full(&self) -> bool {
        self.count >= DEPTH
    }

    /// Copy the NV12 `frame` into the next free slot's surface and submit it as `pic`. The GPU
    /// encodes it while the caller goes on; [`Session::collect`] reads it.
    pub(crate) fn submit(&mut self, frame: &[u8], pic: &Picture) -> Result<(), String> {
        if self.full() {
            return Err("every frame slot of the encoder is in use".into());
        }
        let slot = (self.head + self.count) % DEPTH;
        let input = self.surfaces[slot];
        self.copy_in(frame, input)?;
        let n = self.frames as usize;
        let (write, read) = recon_slots(n);
        let (recon, reference) = (self.surfaces[DEPTH + write], self.surfaces[DEPTH + read]);
        let mut ids = [abi::VA_INVALID_ID; 6];
        let mut made = 0;
        let sent = self.picture(pic, input, recon, reference, self.coded[slot], &mut ids, &mut made);
        for &id in &ids[..made] {
            // SAFETY: each id is a buffer made above on this display, destroyed once; vaEndPicture
            // has returned, and the driver keeps no pointer to a parameter buffer after it.
            unsafe { (self.display.api.destroy_buffer)(self.display.dpy, id) };
        }
        sent?;
        self.count += 1;
        self.frames += 1;
        Ok(())
    }

    /// Make the parameter buffers of `pic` (their ids go into `ids`, `made` of them) and encode
    /// `input` with them.
    fn picture(
        &self,
        pic: &Picture,
        input: VASurfaceID,
        recon: VASurfaceID,
        reference: VASurfaceID,
        coded: VABufferID,
        ids: &mut [VABufferID; 6],
        made: &mut usize,
    ) -> Result<(), String> {
        let ctl = self.choice.ctl;
        let mut push = |id: Result<VABufferID, String>| -> Result<(), String> {
            ids[*made] = id?;
            *made += 1;
            Ok(())
        };
        if pic.idr {
            match self.p.codec {
                Codec::H264 => push(self.value(abi::VAEncSequenceParameterBufferType, &h264_sequence(&self.p, self.choice.profile, ctl)))?,
                Codec::Hevc => push(self.value(abi::VAEncSequenceParameterBufferType, &hevc_sequence(&self.p, self.choice.blocks, ctl)))?,
            }
            if let Some((rc, hrd)) = rate_buffers(ctl) {
                push(self.value(abi::VAEncMiscParameterBufferType, &rc))?;
                push(self.value(abi::VAEncMiscParameterBufferType, &hrd))?;
            }
            push(self.value(abi::VAEncMiscParameterBufferType, &frame_rate_buffer(self.p.fps)))?;
        }
        match self.p.codec {
            Codec::H264 => {
                let profile = self.choice.profile;
                push(self.value(abi::VAEncPictureParameterBufferType, &h264_picture(pic, profile, ctl, recon, reference, coded)))?;
                push(self.value(abi::VAEncSliceParameterBufferType, &h264_slice(pic, self.units, reference)))?;
            }
            Codec::Hevc => {
                push(self.value(abi::VAEncPictureParameterBufferType, &hevc_picture(pic, ctl, recon, reference, coded)))?;
                push(self.value(abi::VAEncSliceParameterBufferType, &hevc_slice(pic, self.units, reference)))?;
            }
        }
        let (api, dpy) = (self.display.api, self.display.dpy);
        // SAFETY: `input` is one of this context's render targets.
        let st = unsafe { (api.begin_picture)(dpy, self.context, input) };
        self.display.check(st, "vaBeginPicture")?;
        let n = c_int::try_from(*made).unwrap_or(0);
        // SAFETY: the first `n` ids are live buffers made on this context.
        let st = unsafe { (api.render_picture)(dpy, self.context, ids.as_mut_ptr(), n) };
        let rendered = self.display.check(st, "vaRenderPicture");
        // A begun picture is always ended, so the context stays usable.
        // SAFETY: the picture was begun on this context above.
        let st = unsafe { (api.end_picture)(dpy, self.context) };
        rendered?;
        self.display.check(st, "vaEndPicture")
    }

    /// A parameter buffer of `ty` holding a copy of `value`.
    fn value<T>(&self, ty: abi::VABufferType, value: &T) -> Result<VABufferID, String> {
        let size = c_uint::try_from(std::mem::size_of::<T>()).map_err(|_| "a parameter buffer too large")?;
        let mut id = abi::VA_INVALID_ID;
        let data = (value as *const T).cast::<c_void>().cast_mut();
        // SAFETY: `data` is `size` readable bytes of a `repr(C)` parameter structure; libva copies
        // them into the new buffer and never writes through the pointer.
        let st = unsafe { (self.display.api.create_buffer)(self.display.dpy, self.context, ty, size, 1, data, &mut id) };
        self.display.check(st, "vaCreateBuffer")?;
        Ok(id)
    }

    /// Copy `frame` into `surface`: through the surface's own memory where the driver maps it,
    /// else through the session's image.
    fn copy_in(&mut self, frame: &[u8], surface: VASurfaceID) -> Result<(), String> {
        let (api, dpy) = (self.display.api, self.display.dpy);
        if matches!(self.upload, Upload::Unknown | Upload::Derive) {
            let mut image = VAImage { image_id: abi::VA_INVALID_ID, buf: abi::VA_INVALID_ID, ..Default::default() };
            // SAFETY: `surface` is one of this session's surfaces; `image` is a live output.
            let st = unsafe { (api.derive_image)(dpy, surface, &mut image) };
            if st == abi::VA_STATUS_SUCCESS {
                let written = if image.format.fourcc == abi::VA_FOURCC_NV12 { Some(self.write(&image, frame)) } else { None };
                // SAFETY: the derived image was made above and is destroyed once; the surface stays.
                unsafe { (api.destroy_image)(dpy, image.image_id) };
                if let Some(written) = written {
                    self.upload = Upload::Derive;
                    return written;
                }
            } else if matches!(self.upload, Upload::Derive) {
                return self.display.check(st, "vaDeriveImage");
            }
            let (cw, ch) = coded_size(self.p.codec, self.p.width, self.p.height);
            let mut format = VAImageFormat { fourcc: abi::VA_FOURCC_NV12, byte_order: 1, bits_per_pixel: 12, ..Default::default() };
            let mut image = VAImage { image_id: abi::VA_INVALID_ID, buf: abi::VA_INVALID_ID, ..Default::default() };
            let (w, h) = (c_int::try_from(cw).unwrap_or(c_int::MAX), c_int::try_from(ch).unwrap_or(c_int::MAX));
            // SAFETY: `format` and `image` are live; the driver fills `image` in.
            let st = unsafe { (api.create_image)(dpy, &mut format, w, h, &mut image) };
            self.display.check(st, "vaCreateImage")?;
            self.upload = Upload::Put(image);
        }
        let Upload::Put(image) = self.upload else { return Err("no way to copy frames into the encoder".into()) };
        self.write(&image, frame)?;
        // The whole coded size, so the padding `write` filled reaches the surface too.
        let (w, h) = coded_size(self.p.codec, self.p.width, self.p.height);
        // SAFETY: the image and the surface were made on this display, both at the coded size
        // `w` × `h`, so the copied rectangle lies within both.
        let st = unsafe { (api.put_image)(dpy, surface, image.image_id, 0, 0, w, h, 0, 0, w, h) };
        self.display.check(st, "vaPutImage")
    }

    /// Copy `frame` into the mapped data of `image`.
    fn write(&self, image: &VAImage, frame: &[u8]) -> Result<(), String> {
        let (api, dpy) = (self.display.api, self.display.dpy);
        let mut ptr: *mut c_void = std::ptr::null_mut();
        // SAFETY: `image.buf` is the data buffer of an image made on this display; `ptr` is a live output.
        let st = unsafe { (api.map_buffer)(dpy, image.buf, &mut ptr) };
        self.display.check(st, "vaMapBuffer")?;
        let copied = if ptr.is_null() || image.num_planes < 2 {
            Err("the surface's image has no NV12 planes".to_string())
        } else {
            // SAFETY: a mapped image buffer holds `data_size` writable bytes at `ptr` until it is
            // unmapped below; the slice does not outlive this block.
            let data = unsafe { std::slice::from_raw_parts_mut(ptr.cast::<u8>(), image.data_size as usize) };
            let [p0, p1, _] = image.pitches.map(|v| v as usize);
            let [o0, o1, _] = image.offsets.map(|v| v as usize);
            let (cw, ch) = coded_size(self.p.codec, self.p.width, self.p.height);
            copy_nv12(frame, self.p.width as usize, self.p.height as usize, [cw as usize, ch as usize], data, [p0, p1], [o0, o1])
        };
        // SAFETY: the buffer was mapped above; `data` is no longer used.
        let st = unsafe { (api.unmap_buffer)(dpy, image.buf) };
        copied?;
        self.display.check(st, "vaUnmapBuffer")
    }

    /// Wait for the oldest frame in flight and append its coded data (Annex B) to `out`. Returns
    /// `false` when no frame is in flight.
    pub(crate) fn collect(&mut self, out: &mut Vec<u8>) -> Result<bool, String> {
        if self.count == 0 {
            return Ok(false);
        }
        let slot = self.head;
        self.head = (self.head + 1) % DEPTH;
        self.count -= 1;
        let (api, dpy) = (self.display.api, self.display.dpy);
        // SAFETY: the surface is one of this session's render targets.
        let st = unsafe { (api.sync_surface)(dpy, self.surfaces[slot]) };
        self.display.check(st, "vaSyncSurface")?;
        let coded = self.coded[slot];
        let mut ptr: *mut c_void = std::ptr::null_mut();
        // SAFETY: `coded` is a coded buffer made on this context; `ptr` is a live output.
        let st = unsafe { (api.map_buffer)(dpy, coded, &mut ptr) };
        self.display.check(st, "vaMapBuffer")?;
        let start = out.len();
        let (cw, ch) = coded_size(self.p.codec, self.p.width, self.p.height);
        let limit = coded_bytes(cw, ch) as usize;
        let mut read = Ok(());
        let mut seg = ptr.cast::<abi::VACodedBufferSegment>().cast_const();
        let mut segments = 0;
        // SAFETY: a mapped coded buffer starts with a list of segments, each valid and aligned
        // until the buffer is unmapped below, linked by `next` and ended by a null pointer, which
        // `as_ref` turns into `None`. The reference is not kept past this iteration.
        while let Some(s) = unsafe { seg.as_ref() } {
            segments += 1;
            if segments > 4096 {
                read = Err("the coded buffer's segment list does not end".to_string());
                break;
            }
            if let Some(why) = segment_error(s.status) {
                read = Err(why.to_string());
                break;
            }
            if s.size > 0 {
                if s.buf.is_null() || s.bit_offset != 0 || out.len() - start + s.size as usize > limit {
                    read = Err("the encoder returned a coded segment this crate cannot read".to_string());
                    break;
                }
                // SAFETY: the segment's `size` bytes at `buf` stay valid until the buffer is
                // unmapped; they are copied out here.
                out.extend_from_slice(unsafe { std::slice::from_raw_parts(s.buf.cast::<u8>().cast_const(), s.size as usize) });
            }
            seg = s.next.cast::<abi::VACodedBufferSegment>().cast_const();
        }
        // SAFETY: the buffer was mapped above; no segment is used after this.
        let st = unsafe { (api.unmap_buffer)(dpy, coded) };
        read?;
        self.display.check(st, "vaUnmapBuffer")?;
        Ok(true)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let (api, dpy) = (self.display.api, self.display.dpy);
        // SAFETY: each object below was made on this display and is destroyed once, by this
        // session only; the display is terminated after this, when its field drops.
        unsafe {
            for &id in &self.coded {
                (api.destroy_buffer)(dpy, id);
            }
            if let Upload::Put(image) = self.upload {
                (api.destroy_image)(dpy, image.image_id);
            }
            if self.context != abi::VA_INVALID_ID {
                (api.destroy_context)(dpy, self.context);
            }
            if !self.surfaces.is_empty() {
                (api.destroy_surfaces)(dpy, self.surfaces.as_mut_ptr(), self.surfaces.len() as c_int);
            }
            if self.config != abi::VA_INVALID_ID {
                (api.destroy_config)(dpy, self.config);
            }
        }
    }
}
