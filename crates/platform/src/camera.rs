//! Cameras: listing them and their formats, and capture.
//!
//! The system's cameras come from Media Foundation on Windows, AVFoundation on macOS and V4L2 on
//! Linux, all behind one [`CameraBackend`]. A camera is streamed by one thread of its own, which
//! opens it, converts each frame once and publishes it into a [`FrameSlot`] for each
//! [`CameraCapture`] reading that camera, so every source thread takes the newest picture without
//! waiting and frames are reused. The camera closes when its last reader is dropped.
//!
//! A camera is stored in settings as its name and the system's id for it ([`CameraInfo::key`]).
//! When that exact camera is gone, capture opens the first camera with the same name.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use studiocraft_media::video::VideoFrame;

use crate::PlatformError;
use crate::camera_frames::Converter;
use crate::screen_frames::{FrameSlot, State};

/// A camera the system reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CameraInfo {
    pub name: String,
    /// The system's id for the camera; stable while it stays plugged in.
    pub id: String,
}

impl CameraInfo {
    /// The key stored in settings.
    pub fn key(&self) -> String {
        format!("{}\u{1f}{}", self.name, self.id)
    }

    pub fn parse_key(key: &str) -> CameraInfo {
        match key.split_once('\u{1f}') {
            Some((name, id)) => CameraInfo { name: name.into(), id: id.into() },
            None => CameraInfo { name: key.into(), id: String::new() },
        }
    }
}

/// The index of the camera to open for `want` in `list`: the same camera, then one with the same
/// id, then one with the same name. An empty `want` takes the first camera. A Linux id names a
/// device node (`/dev/video…`), which another camera can take after an unplug, so an id of
/// that kind matches only together with the name.
pub fn best_match(list: &[CameraInfo], want: &CameraInfo) -> Option<usize> {
    if want.name.is_empty() && want.id.is_empty() {
        return (!list.is_empty()).then_some(0);
    }
    let stable_id = !want.id.is_empty() && !want.id.contains("/dev/");
    list.iter()
        .position(|c| c == want)
        .or_else(|| list.iter().position(|c| stable_id && c.id == want.id))
        .or_else(|| list.iter().position(|c| !want.name.is_empty() && c.name == want.name))
}

/// How a camera encodes its pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CameraPixel {
    /// Each frame a JPEG picture.
    Mjpeg,
    /// Packed 4:2:2 YUV.
    Yuyv,
    /// 4:2:0 YUV: a luma plane and an interleaved chroma plane.
    Nv12,
    Gray,
    Rgb,
    Bgr,
}

const PIXELS: [(CameraPixel, &str, &str); 6] = [
    (CameraPixel::Mjpeg, "MJPG", "MJPEG"),
    (CameraPixel::Yuyv, "YUYV", "YUYV"),
    (CameraPixel::Nv12, "NV12", "NV12"),
    (CameraPixel::Gray, "GREY", "Gray"),
    (CameraPixel::Rgb, "RGB3", "RGB"),
    (CameraPixel::Bgr, "BGR3", "BGR"),
];

impl CameraPixel {
    /// The four-character code stored in settings.
    pub fn code(self) -> &'static str {
        PIXELS.iter().find(|p| p.0 == self).map_or("", |p| p.1)
    }

    pub fn name(self) -> &'static str {
        PIXELS.iter().find(|p| p.0 == self).map_or("", |p| p.2)
    }

    pub fn from_code(code: &str) -> Option<CameraPixel> {
        PIXELS.iter().find(|p| p.1 == code).map(|p| p.0)
    }
}

/// A size, frame rate and pixel encoding a camera offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct CameraFormat {
    pub width: u32,
    pub height: u32,
    /// Frames per second.
    pub fps: u32,
    pub pixel: CameraPixel,
}

impl CameraFormat {
    /// The value stored in settings, such as `1280x720@30:MJPG`.
    pub fn key(&self) -> String {
        format!("{}x{}@{}:{}", self.width, self.height, self.fps, self.pixel.code())
    }

    /// A key written by [`CameraFormat::key`]; `None` for anything else.
    pub fn parse(key: &str) -> Option<CameraFormat> {
        let (size, rest) = key.split_once('@')?;
        let (fps, code) = rest.split_once(':')?;
        let (w, h) = size.split_once('x')?;
        let f = CameraFormat { width: w.parse().ok()?, height: h.parse().ok()?, fps: fps.parse().ok()?, pixel: CameraPixel::from_code(code)? };
        (f.width > 0 && f.height > 0 && f.fps > 0).then_some(f)
    }

    /// The label lists show, such as `1280 x 720, 30 fps, MJPEG`.
    pub fn label(&self) -> String {
        format!("{} x {}, {} fps, {}", self.width, self.height, self.fps, self.pixel.name())
    }
}

/// The format a camera opens with when none is chosen: the largest picture up to 1920 x 1080 at 30
/// frames per second or more, else the largest picture; then the highest frame rate, then the
/// encoding that costs least to convert.
pub fn default_format(formats: &[CameraFormat]) -> Option<CameraFormat> {
    let area = |f: &CameraFormat| u64::from(f.width) * u64::from(f.height);
    let fits = |f: &&CameraFormat| f.width <= 1920 && f.height <= 1080 && f.fps >= 30;
    let pool: Vec<&CameraFormat> = if formats.iter().any(|f| fits(&f)) { formats.iter().filter(fits).collect() } else { formats.iter().collect() };
    let cost = |p: CameraPixel| match p {
        CameraPixel::Rgb | CameraPixel::Bgr => 0,
        CameraPixel::Nv12 | CameraPixel::Yuyv => 1,
        CameraPixel::Gray => 2,
        CameraPixel::Mjpeg => 3,
    };
    pool.into_iter().max_by_key(|f| (area(f), f.fps, std::cmp::Reverse(cost(f.pixel)))).copied()
}

/// The cameras of one system, and opening them. [`system`] is the real one; tests use their own.
pub trait CameraBackend: Send + Sync {
    fn cameras(&self) -> Result<Vec<CameraInfo>, PlatformError>;
    /// The formats `camera` offers.
    fn formats(&self, camera: &CameraInfo) -> Result<Vec<CameraFormat>, PlatformError>;
    /// Start `camera` streaming in `format`. Called on the capture's own thread.
    fn open(&self, camera: &CameraInfo, format: CameraFormat) -> Result<Box<dyn CameraStream>, PlatformError>;
}

/// An open camera, used on the capture thread only.
pub trait CameraStream {
    /// Wait for the next frame and hand its bytes and format to `frame`. A camera that changes its
    /// format while it streams passes the new one.
    fn next(&mut self, frame: &mut dyn FnMut(&[u8], CameraFormat)) -> Result<(), PlatformError>;
}

/// The system's cameras.
pub fn system() -> Arc<dyn CameraBackend> {
    static SYSTEM: OnceLock<Arc<dyn CameraBackend>> = OnceLock::new();
    Arc::clone(SYSTEM.get_or_init(|| Arc::new(imp::System)))
}

/// The system's cameras; empty when they cannot be listed.
pub fn cameras() -> Vec<CameraInfo> {
    system().cameras().unwrap_or_default()
}

/// The formats each camera offered the last time they were read, by camera key.
fn known_formats() -> &'static Mutex<Vec<(String, Vec<CameraFormat>)>> {
    static KNOWN: OnceLock<Mutex<Vec<(String, Vec<CameraFormat>)>>> = OnceLock::new();
    KNOWN.get_or_init(|| Mutex::new(Vec::new()))
}

/// Cameras whose formats are remembered.
const KNOWN_CAMERAS: usize = 32;

/// The cameras in use by this app. The lock is never held while a camera is read.
#[derive(Default)]
struct Cameras {
    /// The cameras whose capture thread is running, with what that thread shares with its
    /// readers. A camera is listed at most once.
    open: Vec<(String, Arc<Shared>)>,
    /// The cameras whose formats are being read outside a capture.
    reading: Vec<String>,
}

/// [`Cameras`], with the condition signaled when a format read ends, a capture opens its camera
/// or ends, and when a reader is dropped.
fn capturing() -> &'static (Mutex<Cameras>, Condvar) {
    static CAMERAS: OnceLock<(Mutex<Cameras>, Condvar)> = OnceLock::new();
    CAMERAS.get_or_init(|| (Mutex::new(Cameras::default()), Condvar::new()))
}

/// Marks a camera's formats as being read while it lives.
struct Reading(String);

impl Drop for Reading {
    fn drop(&mut self) {
        let (lock, done) = capturing();
        if let Ok(mut cameras) = lock.lock() {
            cameras.reading.retain(|k| *k != self.0);
        }
        done.notify_all();
    }
}

/// Reserves a camera for one capture thread while it lives.
struct Capturing(Arc<Shared>);

/// What [`Capturing::reserve`] found for a reader.
enum Reserved {
    /// The reader joined the capture thread that streams the camera.
    Joined,
    /// No capture streams the camera: the calling thread captures it for this reader.
    Owner(Capturing),
    /// The reader was dropped before it joined a capture.
    Dropped,
}

impl Capturing {
    /// Join `reader` to the capture of `key`, or reserve `key` for a new capture with `reader` as
    /// its first reader. A capture that is still opening its camera is waited for, and the reader
    /// joins it once it streams; when it fails or is stopped before it streams, this thread
    /// captures the camera. A camera that streamed and was stopped while its thread still waits
    /// on the camera is an error: "the camera is still closing". Waits while the camera's formats
    /// are being read.
    fn reserve(key: String, reader: &Arc<Reader>) -> Result<Reserved, PlatformError> {
        let cannot = || PlatformError::Failed("the camera capture could not start".into());
        let (lock, done) = capturing();
        let mut cameras = lock.lock().map_err(|_| cannot())?;
        loop {
            if reader.stop.load(Ordering::Acquire) {
                return Ok(Reserved::Dropped);
            }
            if cameras.reading.contains(&key) {
                cameras = done.wait(cameras).map_err(|_| cannot())?;
                continue;
            }
            let Some((_, shared)) = cameras.open.iter().find(|(k, _)| *k == key) else { break };
            let opened = shared.format.get().is_some();
            if opened && shared.stop.load(Ordering::Acquire) {
                return Err(PlatformError::Failed("the camera is still closing".into()));
            }
            if !opened {
                #[cfg(test)]
                reader.waited.store(true, Ordering::Release);
                cameras = done.wait(cameras).map_err(|_| cannot())?;
                continue;
            }
            let shared = Arc::clone(shared);
            shared.join(reader).map_err(|()| cannot())?;
            let _ = reader.capture.set(shared);
            return Ok(Reserved::Joined);
        }
        let shared = Arc::new(Shared {
            readers: Mutex::new(vec![Arc::clone(reader)]),
            changed: AtomicU64::new(0),
            stop: AtomicBool::new(false),
            format: OnceLock::new(),
        });
        cameras.open.push((key, Arc::clone(&shared)));
        let _ = reader.capture.set(Arc::clone(&shared));
        Ok(Reserved::Owner(Capturing(shared)))
    }
}

impl Drop for Capturing {
    fn drop(&mut self) {
        let (lock, done) = capturing();
        if let Ok(mut cameras) = lock.lock()
            && let Some(i) = cameras.open.iter().position(|(_, s)| Arc::ptr_eq(s, &self.0))
        {
            cameras.open.swap_remove(i);
        }
        done.notify_all();
    }
}

/// The formats `camera` offers through `backend`. A camera that a capture of this app has open
/// is not opened again, since reading its formats can change the format it streams in: it gives
/// the formats read when that capture opened it. A camera that cannot be read now (another
/// program has it open) gives the formats it offered when it was last read, or an empty list.
/// A camera whose formats another thread is reading gives the remembered formats too, so the
/// caller does not wait on the camera. A capture cannot reserve the camera while its formats
/// are being read.
pub fn formats_with(backend: &dyn CameraBackend, camera: &CameraInfo) -> Vec<CameraFormat> {
    let key = camera.key();
    {
        let Ok(mut cameras) = capturing().0.lock() else { return Vec::new() };
        if cameras.open.iter().any(|(k, _)| *k == key) || cameras.reading.contains(&key) {
            drop(cameras);
            return remembered(&key);
        }
        cameras.reading.push(key.clone());
    }
    let _reading = Reading(key);
    read_formats(backend, camera)
}

/// The formats the camera `key` offered when it was last read.
fn remembered(key: &str) -> Vec<CameraFormat> {
    known_formats().lock().ok().and_then(|known| known.iter().find(|(k, _)| k == key).map(|(_, l)| l.clone())).unwrap_or_default()
}

/// Read the formats `camera` offers and remember them, or give the ones it offered when it was
/// last read. No lock is held while the camera is read.
fn read_formats(backend: &dyn CameraBackend, camera: &CameraInfo) -> Vec<CameraFormat> {
    let key = camera.key();
    let read = backend.formats(camera);
    let Ok(mut known) = known_formats().lock() else { return read.unwrap_or_default() };
    match read {
        Ok(list) => {
            known.retain(|(k, _)| *k != key);
            if known.len() >= KNOWN_CAMERAS {
                known.remove(0);
            }
            known.push((key, list.clone()));
            list
        }
        Err(_) => known.iter().find(|(k, _)| *k == key).map(|(_, l)| l.clone()).unwrap_or_default(),
    }
}

/// The cameras of `listed` that send pictures: each one `captures` accepts, and each one a capture
/// of this app streams or is opening. A driver can refuse to open a camera that streams, so
/// `captures` is not asked about those.
#[cfg_attr(not(any(windows, target_os = "macos", target_os = "linux")), allow(dead_code))]
fn sending<I>(listed: Vec<(CameraInfo, I)>, captures: impl Fn(&I) -> bool) -> Vec<CameraInfo> {
    let open: Vec<String> = capturing().0.lock().map(|cameras| cameras.open.iter().map(|(k, _)| k.clone()).collect()).unwrap_or_default();
    listed.into_iter().filter(|(c, i)| open.contains(&c.key()) || captures(i)).map(|(c, _)| c).collect()
}

/// [`formats_with`] the system's cameras.
pub fn formats(camera: &CameraInfo) -> Vec<CameraFormat> {
    formats_with(system().as_ref(), camera)
}

/// Run `f`, turning a panic inside a camera library into an error.
fn guarded<T>(f: impl FnOnce() -> Result<T, PlatformError>) -> Result<T, PlatformError> {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| Err(PlatformError::Failed("the camera stopped unexpectedly".into())))
}

/// Frames in a row the capture could not read before it gives up on the camera.
const BAD_FRAMES: u32 = 30;

/// Room for readers that join a camera while its capture waits for a frame.
const SPARE_READERS: usize = 8;

/// What one [`CameraCapture`] shares with the capture thread it reads.
struct Reader {
    slot: FrameSlot,
    /// Set when the [`CameraCapture`] is dropped.
    stop: AtomicBool,
    /// Why the capture failed, once it has.
    error: Mutex<Option<PlatformError>>,
    /// The format this reader asked for; `None` for the camera's default.
    wanted: Option<CameraFormat>,
    /// The capture the reader joined, set once it has.
    capture: OnceLock<Arc<Shared>>,
    /// Whether the reader waited for a camera that was opening.
    #[cfg(test)]
    waited: AtomicBool,
}

impl Reader {
    fn fail(&self, e: PlatformError) {
        if let Ok(mut error) = self.error.lock() {
            error.get_or_insert(e);
        }
        self.slot.set_state(State::Failed);
    }
}

/// What a capture thread shares with its readers.
struct Shared {
    /// The readers the thread publishes each frame to.
    readers: Mutex<Vec<Arc<Reader>>>,
    /// Counts the changes to `readers`, so the thread copies the list only when it changed.
    changed: AtomicU64,
    /// Set when the last reader leaves.
    stop: AtomicBool,
    /// The format the camera was opened in, once it streams.
    format: OnceLock<CameraFormat>,
}

impl Shared {
    /// Add `reader`; `Err` when the list cannot be locked.
    fn join(&self, reader: &Arc<Reader>) -> Result<(), ()> {
        self.readers.lock().map_err(|_| ())?.push(Arc::clone(reader));
        self.changed.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// Remove `reader`; the thread stops after its current frame once no reader is left.
    fn leave(&self, reader: &Arc<Reader>) {
        if let Ok(mut readers) = self.readers.lock() {
            readers.retain(|r| !Arc::ptr_eq(r, reader));
            if readers.is_empty() {
                self.stop.store(true, Ordering::Release);
            }
        } else {
            self.stop.store(true, Ordering::Release);
        }
        self.changed.fetch_add(1, Ordering::AcqRel);
    }

    /// Fail every reader with `e`.
    fn fail(&self, e: &PlatformError) {
        if let Ok(readers) = self.readers.lock() {
            for r in readers.iter() {
                r.fail(e.clone());
            }
        }
    }
}

/// A camera being read. Several captures of one camera share its capture thread: each gets every
/// frame, converted once. Dropping the last capture of a camera stops its thread after the
/// current frame.
pub struct CameraCapture {
    reader: Arc<Reader>,
}

impl CameraCapture {
    /// Capture the system camera `key` ([`CameraInfo::key`]; empty for the first camera) in
    /// `format`, or in [`default_format`] when it is `None`. Returns at once; the camera opens on
    /// the capture thread. A camera that another capture of this app already reads is shared in
    /// the format it was opened in ([`CameraCapture::other_format`]).
    pub fn open(key: &str, format: Option<CameraFormat>) -> Result<CameraCapture, PlatformError> {
        CameraCapture::open_with(system(), key, format)
    }

    /// [`CameraCapture::open`] with the cameras of `backend`.
    pub fn open_with(backend: Arc<dyn CameraBackend>, key: &str, format: Option<CameraFormat>) -> Result<CameraCapture, PlatformError> {
        let reader = Arc::new(Reader {
            slot: FrameSlot::default(),
            stop: AtomicBool::new(false),
            error: Mutex::new(None),
            wanted: format,
            capture: OnceLock::new(),
            #[cfg(test)]
            waited: AtomicBool::new(false),
        });
        let (theirs, want) = (Arc::clone(&reader), CameraInfo::parse_key(key));
        // The thread is not joined: a camera can keep it waiting for a frame, and it ends on its
        // own once it sees the stop flag or its camera fails. Until then the camera stays
        // reserved for it (`Capturing`). A thread whose reader joins another capture ends at
        // once.
        std::thread::Builder::new()
            .name("camera capture".into())
            .spawn(move || {
                if let Err(e) = guarded(|| run(backend.as_ref(), &want, &theirs)) {
                    theirs.fail(e);
                }
            })
            .map_err(|_| PlatformError::Failed("the camera capture could not start".into()))?;
        Ok(CameraCapture { reader })
    }

    /// The newest picture into `frame` (RGBA, or BGRA for BGR cameras). [`PlatformError::Waiting`]
    /// until the camera sends its first frame, [`PlatformError::Unchanged`] while no newer frame
    /// has arrived, and the reason once the capture failed.
    pub fn grab(&mut self, frame: &mut VideoFrame) -> Result<(), PlatformError> {
        if self.reader.slot.take(frame) {
            return Ok(());
        }
        match self.reader.slot.state() {
            State::Waiting => Err(PlatformError::Waiting),
            State::Streaming => Err(PlatformError::Unchanged),
            _ => Err(self.reader.error.lock().ok().and_then(|e| e.clone()).unwrap_or_else(|| PlatformError::Failed("the camera stopped".into()))),
        }
    }

    /// The format the camera streams in, when this capture asked for another one: the camera was
    /// already open in that format for another capture, and this one shares it.
    pub fn other_format(&self) -> Option<CameraFormat> {
        let open = *self.reader.capture.get()?.format.get()?;
        self.reader.wanted.filter(|w| *w != open).map(|_| open)
    }
}

impl Drop for CameraCapture {
    fn drop(&mut self) {
        self.reader.stop.store(true, Ordering::Release);
        let (lock, done) = capturing();
        // Under the lock, so a reader cannot join a capture whose last reader is leaving.
        let cameras = lock.lock();
        if let Some(shared) = self.reader.capture.get() {
            shared.leave(&self.reader);
        }
        drop(cameras);
        done.notify_all();
    }
}

/// The capture thread: find the camera and join the capture that streams it, or open it and
/// convert frames until its last reader is dropped. Returns `Ok` once the reader joined another
/// capture, or when stopped; a failure of the stream fails every reader.
fn run(backend: &dyn CameraBackend, want: &CameraInfo, reader: &Arc<Reader>) -> Result<(), PlatformError> {
    let list = backend.cameras()?;
    let camera = best_match(&list, want).and_then(|i| list.get(i)).ok_or_else(|| {
        PlatformError::NotFound(if want.name.is_empty() { "a camera".into() } else { format!("the camera \u{201c}{}\u{201d}", want.name) })
    })?;
    let capturing = match Capturing::reserve(camera.key(), reader)? {
        Reserved::Joined | Reserved::Dropped => return Ok(()),
        Reserved::Owner(c) => c,
    };
    let shared = Arc::clone(&capturing.0);
    let streamed = guarded(|| stream(backend, camera, reader.wanted, &shared));
    // The camera is free for a new capture before the readers learn of the failure, so a
    // reader that opens it again does not join this one.
    drop(capturing);
    if let Err(e) = streamed {
        shared.fail(&e);
    }
    Ok(())
}

/// Open `camera` in `format` (the default one when `None`) and publish its frames to the readers
/// of `shared` until it is stopped.
fn stream(backend: &dyn CameraBackend, camera: &CameraInfo, format: Option<CameraFormat>, shared: &Shared) -> Result<(), PlatformError> {
    let offered = read_formats(backend, camera);
    let format = match format {
        Some(f) if offered.is_empty() || offered.contains(&f) => f,
        Some(f) => return Err(PlatformError::Failed(format!("the camera does not offer {}", f.label()))),
        None => default_format(&offered).ok_or_else(|| PlatformError::Failed("the camera offers no format StudioCraft reads".into()))?,
    };
    // A capture stopped before its camera opened does not open it: a reader waiting for it
    // captures the camera instead.
    if shared.stop.load(Ordering::Acquire) {
        return Ok(());
    }
    let mut stream = backend.open(camera, format)?;
    {
        // Readers waiting for the camera to open join now. Readers leave under this lock, so a
        // capture whose last reader left while the camera opened closes it without streaming.
        let (lock, done) = capturing();
        let cameras = lock.lock();
        if shared.stop.load(Ordering::Acquire) {
            return Ok(());
        }
        let _ = shared.format.set(format);
        drop(cameras);
        done.notify_all();
    }
    let (mut bad, mut converter) = (0u32, Converter::default());
    let (mut readers, mut seen): (Vec<Arc<Reader>>, Option<u64>) = (Vec::with_capacity(SPARE_READERS), None);
    let mut sized = None;
    while !shared.stop.load(Ordering::Acquire) {
        // The reader list grows here, between frames, so the frame callback does not allocate.
        // A busy list is sized at the next frame.
        let changed = shared.changed.load(Ordering::Acquire);
        if sized != Some(changed)
            && let Ok(list) = shared.readers.try_lock()
        {
            readers.reserve((list.len() + SPARE_READERS).saturating_sub(readers.len()));
            sized = Some(changed);
        }
        stream.next(&mut |bytes, format| {
            // Read when the frame arrives, so a reader that joined during the wait gets it. The
            // list is read without waiting and only when it fits: otherwise this frame goes to
            // the readers read before, and the next frame takes the new list.
            let changed = shared.changed.load(Ordering::Acquire);
            if seen != Some(changed)
                && let Ok(list) = shared.readers.try_lock()
                && list.len() <= readers.capacity()
            {
                readers.clear();
                readers.extend(list.iter().cloned());
                seen = Some(changed);
            }
            let Some((first, rest)) = readers.split_first() else { return };
            let mut frame = first.slot.spare();
            if converter.convert(bytes, format, &mut frame) {
                bad = 0;
                for r in rest {
                    let mut copy = r.slot.spare();
                    copy_picture(&frame, &mut copy);
                    r.slot.publish(copy);
                }
                first.slot.publish(frame);
            } else {
                bad = bad.saturating_add(1);
                first.slot.recycle(frame);
            }
        })?;
        if bad >= BAD_FRAMES {
            return Err(PlatformError::Failed("the camera sends frames that cannot be read".into()));
        }
    }
    Ok(())
}

/// Copy the picture of `from` into `to`, reusing `to`'s buffer.
fn copy_picture(from: &VideoFrame, to: &mut VideoFrame) {
    to.width = from.width;
    to.height = from.height;
    to.format = from.format;
    to.pts = from.pts;
    to.data.clear();
    to.data.extend_from_slice(&from.data);
}

/// Media Foundation, AVFoundation and V4L2 cameras.
#[cfg(any(windows, target_os = "macos", target_os = "linux"))]
mod imp {
    use std::borrow::Cow;

    use nokhwa::Camera;
    use nokhwa::utils::{ApiBackend, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType};

    use super::{CameraBackend, CameraFormat, CameraInfo, CameraPixel, CameraStream, best_match, guarded};
    use crate::PlatformError;

    const ENCODINGS: [FrameFormat; 6] =
        [FrameFormat::MJPEG, FrameFormat::YUYV, FrameFormat::NV12, FrameFormat::GRAY, FrameFormat::RAWRGB, FrameFormat::RAWBGR];

    fn pixel(f: FrameFormat) -> CameraPixel {
        match f {
            FrameFormat::MJPEG => CameraPixel::Mjpeg,
            FrameFormat::YUYV => CameraPixel::Yuyv,
            FrameFormat::NV12 => CameraPixel::Nv12,
            FrameFormat::GRAY => CameraPixel::Gray,
            FrameFormat::RAWRGB => CameraPixel::Rgb,
            FrameFormat::RAWBGR => CameraPixel::Bgr,
        }
    }

    fn encoding(p: CameraPixel) -> FrameFormat {
        match p {
            CameraPixel::Mjpeg => FrameFormat::MJPEG,
            CameraPixel::Yuyv => FrameFormat::YUYV,
            CameraPixel::Nv12 => FrameFormat::NV12,
            CameraPixel::Gray => FrameFormat::GRAY,
            CameraPixel::Rgb => FrameFormat::RAWRGB,
            CameraPixel::Bgr => FrameFormat::RAWBGR,
        }
    }

    fn ours(f: &nokhwa::utils::CameraFormat) -> CameraFormat {
        CameraFormat { width: f.width(), height: f.height(), fps: f.frame_rate(), pixel: pixel(f.format()) }
    }

    fn failed(what: &str, e: impl std::fmt::Display) -> PlatformError {
        PlatformError::Failed(format!("{what} ({e})"))
    }

    /// COM for the calling thread, in a single-threaded apartment as the camera library sets up
    /// its own first thread. Media Foundation needs COM on every thread that uses it, and the
    /// library sets it up on one thread only.
    #[cfg(windows)]
    fn com() -> Result<comedy::com::ComApartmentScope, PlatformError> {
        comedy::com::ComApartmentScope::init_sta().map_err(|e| failed("the camera library could not start", e))
    }

    /// Run `f` on a thread of its own with COM set up (Windows), or on this thread.
    fn on_com_thread<T: Send>(f: impl FnOnce() -> Result<T, PlatformError> + Send) -> Result<T, PlatformError> {
        #[cfg(windows)]
        {
            std::thread::scope(|s| {
                s.spawn(|| {
                    let _com = com()?;
                    guarded(f)
                })
                .join()
                .unwrap_or_else(|_| Err(PlatformError::Failed("the camera stopped unexpectedly".into())))
            })
        }
        #[cfg(not(windows))]
        {
            guarded(f)
        }
    }

    /// On macOS the person allows camera access once; until then nothing is listed or opened.
    #[cfg(target_os = "macos")]
    fn allowed() -> Result<(), PlatformError> {
        static ASKED: std::sync::Once = std::sync::Once::new();
        if nokhwa::nokhwa_check() {
            return Ok(());
        }
        ASKED.call_once(|| nokhwa::nokhwa_initialize(|_| {}));
        Err(PlatformError::Failed("camera access is not allowed in the system's privacy settings".into()))
    }

    #[cfg(not(target_os = "macos"))]
    fn allowed() -> Result<(), PlatformError> {
        Ok(())
    }

    /// The encodings a camera sends as listed. On macOS the library asks the system for 10-bit
    /// samples when NV12 is chosen, gray with white as zero, and lists the 4:2:0 formats as YUYV;
    /// YUYV makes the system convert any camera format to 8-bit 4:2:2, so only YUYV is offered.
    #[cfg(target_os = "macos")]
    const SENDS: &[CameraPixel] = &[CameraPixel::Yuyv];
    #[cfg(not(target_os = "macos"))]
    const SENDS: &[CameraPixel] = &[CameraPixel::Mjpeg, CameraPixel::Yuyv, CameraPixel::Nv12, CameraPixel::Gray, CameraPixel::Rgb, CameraPixel::Bgr];

    /// The row stride and picture size in bytes the V4L2 node `index` sends at, read while it
    /// streams. A mapped V4L2 buffer can be longer than the picture.
    #[cfg(target_os = "linux")]
    fn layout(index: &CameraIndex) -> Option<(usize, usize)> {
        use v4l::video::Capture;
        let i = usize::try_from(index.as_index().ok()?).ok()?;
        let f = v4l::Device::new(i).and_then(|d| d.format()).ok()?;
        Some((usize::try_from(f.stride).ok()?, usize::try_from(f.size).ok()?))
    }

    #[cfg(not(target_os = "linux"))]
    fn layout(_index: &CameraIndex) -> Option<(usize, usize)> {
        None
    }

    /// The system's cameras with the library's index for each.
    fn listed() -> Result<Vec<(CameraInfo, CameraIndex)>, PlatformError> {
        allowed()?;
        let list = nokhwa::query(ApiBackend::Auto).map_err(|e| failed("the cameras could not be listed", e))?;
        Ok(list
            .into_iter()
            .map(|c| {
                // The device path on Linux, the device's link on Windows, its unique id on macOS.
                let id = if c.misc().is_empty() { c.description().to_string() } else { c.misc() };
                (CameraInfo { name: c.human_name(), id }, c.index().clone())
            })
            .collect())
    }

    /// Whether the V4L2 node `index` sends pictures: a camera also has nodes that carry only
    /// metadata. Asking does not disturb a camera that is streaming.
    #[cfg(target_os = "linux")]
    fn captures(index: &CameraIndex) -> bool {
        let Ok(i) = index.as_index() else { return false };
        let Ok(i) = usize::try_from(i) else { return false };
        v4l::Device::new(i).and_then(|d| d.query_caps()).is_ok_and(|c| c.capabilities.contains(v4l::capability::Flags::VIDEO_CAPTURE))
    }

    #[cfg(not(target_os = "linux"))]
    fn captures(_index: &CameraIndex) -> bool {
        true
    }

    fn index_of(camera: &CameraInfo) -> Result<CameraIndex, PlatformError> {
        let list = listed()?;
        let infos: Vec<CameraInfo> = list.iter().map(|(c, _)| c.clone()).collect();
        best_match(&infos, camera)
            .and_then(|i| list.into_iter().nth(i))
            .map(|(_, index)| index)
            .ok_or_else(|| PlatformError::NotFound(format!("the camera \u{201c}{}\u{201d}", camera.name)))
    }

    fn opened(index: CameraIndex, format: RequestedFormatType) -> Result<Camera, PlatformError> {
        Camera::new(index, RequestedFormat::with_formats(format, &ENCODINGS)).map_err(|e| failed("the camera could not be opened", e))
    }

    /// The formats the camera at `index` offers. On macOS they are read from the device itself,
    /// which does not change its active format.
    #[cfg(target_os = "macos")]
    fn read_formats(index: CameraIndex) -> Result<Vec<CameraFormat>, PlatformError> {
        let device = nokhwa_bindings_macos::AVCaptureDevice::new(&index).map_err(|e| failed("the camera could not be opened", e))?;
        let formats = device.supported_formats().map_err(|e| failed("the camera's formats could not be read", e))?;
        Ok(offered(formats.iter().map(ours).collect()))
    }

    #[cfg(not(target_os = "macos"))]
    fn read_formats(index: CameraIndex) -> Result<Vec<CameraFormat>, PlatformError> {
        let gray = gray_formats(&index);
        let listed = opened(index, RequestedFormatType::None)
            .and_then(|mut camera| camera.compatible_camera_formats().map_err(|e| failed("the camera's formats could not be read", e)));
        with_gray(listed.map(|formats| formats.iter().map(ours).collect()), gray)
    }

    /// The formats the camera library lists, with the gray formats read on their own. A camera
    /// that sends only gray cannot be opened by the library, and lists its gray formats.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub(super) fn with_gray(listed: Result<Vec<CameraFormat>, PlatformError>, gray: Vec<CameraFormat>) -> Result<Vec<CameraFormat>, PlatformError> {
        match listed {
            Ok(mut list) => {
                list.extend(gray);
                Ok(offered(list))
            }
            Err(_) if !gray.is_empty() => Ok(offered(gray)),
            Err(e) => Err(e),
        }
    }

    /// The gray formats of the V4L2 node `index`.
    #[cfg(target_os = "linux")]
    fn gray_formats(index: &CameraIndex) -> Vec<CameraFormat> {
        node(index).map(crate::camera_gray::formats).unwrap_or_default()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    fn gray_formats(_index: &CameraIndex) -> Vec<CameraFormat> {
        Vec::new()
    }

    /// The V4L2 node number of `index`.
    #[cfg(target_os = "linux")]
    fn node(index: &CameraIndex) -> Option<usize> {
        usize::try_from(index.as_index().ok()?).ok()
    }

    fn offered(mut list: Vec<CameraFormat>) -> Vec<CameraFormat> {
        list.retain(|f| f.width > 0 && f.height > 0 && f.fps > 0 && SENDS.contains(&f.pixel));
        list.sort_by_key(|f| (std::cmp::Reverse(u64::from(f.width) * u64::from(f.height)), std::cmp::Reverse(f.fps), f.pixel));
        list.dedup();
        list
    }

    pub struct System;

    impl CameraBackend for System {
        fn cameras(&self) -> Result<Vec<CameraInfo>, PlatformError> {
            on_com_thread(|| Ok(super::sending(listed()?, captures)))
        }

        fn formats(&self, camera: &CameraInfo) -> Result<Vec<CameraFormat>, PlatformError> {
            let camera = camera.clone();
            on_com_thread(move || read_formats(index_of(&camera)?))
        }

        /// Called on the capture thread, which keeps the stream.
        fn open(&self, camera: &CameraInfo, format: CameraFormat) -> Result<Box<dyn CameraStream>, PlatformError> {
            guarded(|| {
                #[cfg(windows)]
                let com = com()?;
                let index = index_of(camera)?;
                #[cfg(target_os = "linux")]
                if format.pixel == CameraPixel::Gray {
                    let node = node(&index).ok_or_else(|| PlatformError::Failed("the camera could not be opened".into()))?;
                    return crate::camera_gray::open(node, format);
                }
                let exact = nokhwa::utils::CameraFormat::new_from(format.width, format.height, encoding(format.pixel), format.fps);
                let mut cam = opened(index.clone(), RequestedFormatType::Exact(exact))?;
                cam.open_stream().map_err(|e| failed("the camera could not start", e))?;
                Ok(Box::new(Stream {
                    format: ours(&cam.camera_format()),
                    layout: layout(&index),
                    cam,
                    rows: Vec::new(),
                    #[cfg(windows)]
                    _com: com,
                }) as Box<dyn CameraStream>)
            })
        }
    }

    struct Stream {
        cam: Camera,
        format: CameraFormat,
        /// The row stride and picture size, where the system reports them (Linux).
        layout: Option<(usize, usize)>,
        /// The rows of a frame that needed reordering.
        rows: Vec<u8>,
        /// Dropped after the camera, on the capture thread.
        #[cfg(windows)]
        _com: comedy::com::ComApartmentScope,
    }

    /// Media Foundation stores RGB frames last row first. The other encodings and the other
    /// systems store the top row first. Any system can pad rows (`unpad`).
    const BGR_BOTTOM_UP: bool = cfg!(windows);

    impl CameraStream for Stream {
        fn next(&mut self, frame: &mut dyn FnMut(&[u8], CameraFormat)) -> Result<(), PlatformError> {
            let Stream { cam, format, layout, rows, .. } = self;
            // On macOS the library queues every frame without a bound; `frame` takes one and drops
            // the rest, so a slow conversion does not let the queue grow. The frame carries the
            // size and encoding it was taken at.
            #[cfg(target_os = "macos")]
            let taken = cam.frame().map_err(|e| failed("the camera stopped", e))?;
            #[cfg(target_os = "macos")]
            {
                let r = taken.resolution();
                *format = CameraFormat { width: r.width(), height: r.height(), pixel: pixel(taken.source_frame_format()), ..*format };
            }
            #[cfg(target_os = "macos")]
            let bytes: Cow<'_, [u8]> = Cow::Borrowed(taken.buffer());
            #[cfg(not(target_os = "macos"))]
            let bytes: Cow<'_, [u8]> = cam.frame_raw().map_err(|e| failed("the camera stopped", e))?;
            let format = *format;
            let (stride, size) = layout.map_or((None, bytes.len()), |(stride, size)| (Some(stride), size));
            let bytes = bytes.get(..size).filter(|_| size > 0).unwrap_or(&bytes);
            let bottom_up = BGR_BOTTOM_UP && format.pixel == CameraPixel::Bgr;
            if crate::camera_frames::unpad(bytes, format, stride, bottom_up, rows) {
                frame(rows, format);
            } else {
                frame(bytes, format);
            }
            Ok(())
        }
    }

    impl Drop for Stream {
        fn drop(&mut self) {
            let _ = self.cam.stop_stream();
        }
    }
}

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
    use super::{CameraBackend, CameraFormat, CameraInfo, CameraStream};
    use crate::PlatformError;

    pub struct System;

    impl CameraBackend for System {
        fn cameras(&self) -> Result<Vec<CameraInfo>, PlatformError> {
            Ok(Vec::new())
        }
        fn formats(&self, _camera: &CameraInfo) -> Result<Vec<CameraFormat>, PlatformError> {
            Err(PlatformError::Unsupported)
        }
        fn open(&self, _camera: &CameraInfo, _format: CameraFormat) -> Result<Box<dyn CameraStream>, PlatformError> {
            Err(PlatformError::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use studiocraft_media::video::PixelFormat;

    use super::*;

    fn cam(name: &str, id: &str) -> CameraInfo {
        CameraInfo { name: name.into(), id: id.into() }
    }

    const HD: CameraFormat = CameraFormat { width: 1280, height: 720, fps: 30, pixel: CameraPixel::Yuyv };

    #[test]
    fn keys_round_trip() {
        let c = cam("Desk Camera", "/dev/video2");
        assert_eq!(CameraInfo::parse_key(&c.key()), c);
        assert_eq!(CameraInfo::parse_key("Desk Camera"), cam("Desk Camera", ""));
        assert_eq!(CameraFormat::parse(&HD.key()), Some(HD));
        assert_eq!(HD.key(), "1280x720@30:YUYV");
        assert_eq!(HD.label(), "1280 x 720, 30 fps, YUYV");
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn a_camera_that_sends_only_gray_lists_its_gray_formats() {
        let gray = CameraFormat { width: 640, height: 480, fps: 30, pixel: CameraPixel::Gray };
        let unopened = || Err(PlatformError::Failed("the camera could not be opened (no format)".into()));
        assert_eq!(imp::with_gray(unopened(), vec![gray]), Ok(vec![gray]), "the library cannot open it; its gray formats are listed");
        assert_eq!(imp::with_gray(Ok(vec![gray, HD]), vec![gray]), Ok(vec![HD, gray]), "listed once, largest first");
        assert_eq!(imp::with_gray(unopened(), Vec::new()), unopened(), "a camera with nothing to list is an error");
    }

    #[test]
    fn hostile_format_keys_are_refused() {
        for key in [
            "",
            "1280x720",
            "1280x720@30",
            "1280x720@30:XXXX",
            "0x720@30:YUYV",
            "1280x720@0:YUYV",
            "-1x720@30:YUYV",
            "99999999999x1@1:YUYV",
            "axb@c:YUYV",
        ] {
            assert_eq!(CameraFormat::parse(key), None, "{key}");
        }
    }

    #[test]
    fn matching_prefers_the_same_camera_then_its_id_then_its_name() {
        let list = [cam("A", "1"), cam("B", "2"), cam("A", "3")];
        assert_eq!(best_match(&list, &cam("A", "3")), Some(2));
        assert_eq!(best_match(&list, &cam("Renamed", "2")), Some(1));
        assert_eq!(best_match(&list, &cam("A", "replugged")), Some(0));
        assert_eq!(best_match(&list, &cam("Z", "9")), None);
        assert_eq!(best_match(&list, &cam("", "")), Some(0), "no choice takes the first camera");
        assert_eq!(best_match(&[], &cam("", "")), None);
        // Another camera on the node a saved one used is not that camera.
        let nodes = [cam("Desk", "/dev/video0")];
        assert_eq!(best_match(&nodes, &cam("Room", "/dev/video0")), None);
        assert_eq!(best_match(&nodes, &cam("Desk", "/dev/video2")), Some(0), "the same camera on another node");
        let described = [cam("Desk", "Video4Linux Device @ /dev/video0")];
        assert_eq!(best_match(&described, &cam("Room", "Video4Linux Device @ /dev/video0")), None);
    }

    #[test]
    fn the_default_format_is_the_largest_up_to_1080p30() {
        let f = |width, height, fps, pixel| CameraFormat { width, height, fps, pixel };
        let list = [
            f(3840, 2160, 30, CameraPixel::Mjpeg),
            f(1920, 1080, 30, CameraPixel::Mjpeg),
            f(1920, 1080, 5, CameraPixel::Yuyv),
            f(1280, 720, 60, CameraPixel::Mjpeg),
            f(1920, 1080, 30, CameraPixel::Nv12),
        ];
        assert_eq!(default_format(&list), Some(f(1920, 1080, 30, CameraPixel::Nv12)));
        // With nothing at 30 frames per second, the largest picture.
        assert_eq!(
            default_format(&[f(640, 480, 15, CameraPixel::Yuyv), f(320, 240, 15, CameraPixel::Yuyv)]),
            Some(f(640, 480, 15, CameraPixel::Yuyv))
        );
        assert_eq!(default_format(&[]), None);
    }

    /// What a fake camera does each time it is asked for a frame.
    #[derive(Clone)]
    enum Step {
        Frame(Vec<u8>, CameraFormat),
        Fail(&'static str),
    }

    /// A camera fed by a channel, so a test decides when each frame arrives.
    struct Fake {
        list: Vec<CameraInfo>,
        formats: Result<Vec<CameraFormat>, PlatformError>,
        open: Result<(), PlatformError>,
        steps: Mutex<Option<mpsc::Receiver<Step>>>,
        opened: Mutex<Vec<(CameraInfo, CameraFormat)>>,
        dropped: Arc<AtomicBool>,
        /// How many times the formats were read.
        reads: std::sync::atomic::AtomicUsize,
        /// How many times the stream started waiting for a step.
        waits: Arc<std::sync::atomic::AtomicUsize>,
        /// When set, the first format read waits for a message on it.
        formats_gate: Mutex<Option<mpsc::Receiver<()>>>,
    }

    impl Fake {
        /// Two cameras, Desk and Room, with ids of their own: tests run at once, and every
        /// capture of one camera id in the process shares that camera's capture thread.
        fn new(steps: mpsc::Receiver<Step>) -> Fake {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            Fake {
                list: vec![cam("Desk", &format!("fake-{n}-1")), cam("Room", &format!("fake-{n}-2"))],
                formats: Ok(vec![HD, CameraFormat { width: 2, height: 1, fps: 30, pixel: CameraPixel::Gray }]),
                open: Ok(()),
                steps: Mutex::new(Some(steps)),
                opened: Mutex::new(Vec::new()),
                dropped: Arc::new(AtomicBool::new(false)),
                reads: std::sync::atomic::AtomicUsize::new(0),
                waits: Arc::default(),
                formats_gate: Mutex::new(None),
            }
        }

        /// Wait until the stream waits for its `n`th step.
        fn until_waiting(&self, n: usize) {
            while self.waits.load(Ordering::Acquire) < n {
                std::thread::yield_now();
            }
        }
    }

    struct FakeStream {
        steps: mpsc::Receiver<Step>,
        dropped: Arc<AtomicBool>,
        waits: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Drop for FakeStream {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    impl CameraStream for FakeStream {
        fn next(&mut self, frame: &mut dyn FnMut(&[u8], CameraFormat)) -> Result<(), PlatformError> {
            self.waits.fetch_add(1, Ordering::AcqRel);
            match self.steps.recv() {
                Ok(Step::Frame(bytes, format)) => {
                    frame(&bytes, format);
                    Ok(())
                }
                Ok(Step::Fail(why)) => Err(PlatformError::Failed(why.into())),
                Err(_) => Err(PlatformError::Failed("unplugged".into())),
            }
        }
    }

    impl CameraBackend for Fake {
        fn cameras(&self) -> Result<Vec<CameraInfo>, PlatformError> {
            Ok(self.list.clone())
        }
        fn formats(&self, _camera: &CameraInfo) -> Result<Vec<CameraFormat>, PlatformError> {
            let gate = self.formats_gate.lock().unwrap().take();
            self.reads.fetch_add(1, Ordering::AcqRel);
            if let Some(gate) = gate {
                gate.recv().unwrap();
            }
            self.formats.clone()
        }
        fn open(&self, camera: &CameraInfo, format: CameraFormat) -> Result<Box<dyn CameraStream>, PlatformError> {
            self.open.clone()?;
            self.opened.lock().unwrap().push((camera.clone(), format));
            let steps = self.steps.lock().unwrap().take().ok_or(PlatformError::Failed("opened twice".into()))?;
            Ok(Box::new(FakeStream { steps, dropped: Arc::clone(&self.dropped), waits: Arc::clone(&self.waits) }))
        }
    }

    const GRAY: CameraFormat = CameraFormat { width: 2, height: 1, fps: 30, pixel: CameraPixel::Gray };

    /// Grab until the capture gives something other than `Waiting`, handing over each step first.
    fn grab_after(c: &mut CameraCapture, tx: &mpsc::Sender<Step>, step: Step, f: &mut VideoFrame) -> Result<(), PlatformError> {
        tx.send(step).unwrap();
        loop {
            match c.grab(f) {
                Err(PlatformError::Waiting) | Err(PlatformError::Unchanged) => std::thread::yield_now(),
                other => return other,
            }
        }
    }

    #[test]
    fn frames_arrive_converted_and_reuse_their_buffers() {
        let (tx, rx) = mpsc::channel();
        let fake = Arc::new(Fake::new(rx));
        let room = fake.list[1].clone();
        let mut c = CameraCapture::open_with(fake.clone(), &room.key(), Some(GRAY)).unwrap();
        let mut f = VideoFrame::default();
        assert_eq!(c.grab(&mut f), Err(PlatformError::Waiting), "nothing before the first frame");
        grab_after(&mut c, &tx, Step::Frame(vec![10, 20], GRAY), &mut f).unwrap();
        assert_eq!((f.width, f.height, f.format), (2, 1, PixelFormat::Rgba8));
        assert_eq!(f.data, [10, 10, 10, 255, 20, 20, 20, 255]);
        assert_eq!(c.grab(&mut f), Err(PlatformError::Unchanged), "each frame is taken once");
        assert_eq!(fake.opened.lock().unwrap().as_slice(), &[(room, GRAY)]);
        // A format change while streaming: the next frame comes at its new size.
        let wide = CameraFormat { width: 4, height: 1, ..GRAY };
        grab_after(&mut c, &tx, Step::Frame(vec![1, 2, 3, 4], wide), &mut f).unwrap();
        assert_eq!((f.width, f.data.len()), (4, 16));
    }

    #[test]
    fn no_chosen_format_opens_the_default_one() {
        let (_tx, rx) = mpsc::channel();
        let fake = Arc::new(Fake::new(rx));
        let mut c = CameraCapture::open_with(fake.clone(), "", None).unwrap();
        let mut f = VideoFrame::default();
        while fake.opened.lock().unwrap().is_empty() {
            assert_eq!(c.grab(&mut f), Err(PlatformError::Waiting));
            std::thread::yield_now();
        }
        assert_eq!(fake.opened.lock().unwrap().as_slice(), &[(fake.list[0].clone(), HD)], "the first camera, at its default format");
    }

    fn failure(fake: Fake, key: &str, format: Option<CameraFormat>) -> PlatformError {
        let mut c = CameraCapture::open_with(Arc::new(fake), key, format).unwrap();
        let mut f = VideoFrame::default();
        loop {
            match c.grab(&mut f) {
                Err(PlatformError::Waiting) => std::thread::yield_now(),
                Err(e) => return e,
                Ok(()) => panic!("no frame was sent"),
            }
        }
    }

    #[test]
    fn a_missing_busy_or_unfitting_camera_is_an_error() {
        let (_tx, rx) = mpsc::channel();
        assert_eq!(failure(Fake::new(rx), &cam("Gone", "9").key(), None), PlatformError::NotFound("the camera \u{201c}Gone\u{201d}".into()));
        let (_tx, rx) = mpsc::channel();
        let busy = Fake { open: Err(PlatformError::Failed("the camera could not be opened (in use)".into())), ..Fake::new(rx) };
        assert_eq!(failure(busy, "", None), PlatformError::Failed("the camera could not be opened (in use)".into()));
        let (_tx, rx) = mpsc::channel();
        let odd = CameraFormat { width: 999, height: 9, fps: 7, pixel: CameraPixel::Rgb };
        assert_eq!(failure(Fake::new(rx), "", Some(odd)), PlatformError::Failed("the camera does not offer 999 x 9, 7 fps, RGB".into()));
        let (_tx, rx) = mpsc::channel();
        let none = Fake { formats: Ok(Vec::new()), ..Fake::new(rx) };
        assert_eq!(failure(none, "", None), PlatformError::Failed("the camera offers no format StudioCraft reads".into()));
        let (_tx, rx) = mpsc::channel();
        let empty = Fake { list: Vec::new(), ..Fake::new(rx) };
        assert_eq!(failure(empty, "", None), PlatformError::NotFound("a camera".into()));
    }

    #[test]
    fn an_unplugged_camera_fails_the_capture_and_closes_the_stream() {
        let (tx, rx) = mpsc::channel();
        let fake = Arc::new(Fake::new(rx));
        let dropped = Arc::clone(&fake.dropped);
        let mut c = CameraCapture::open_with(fake, "", Some(GRAY)).unwrap();
        let mut f = VideoFrame::default();
        let e = grab_after(&mut c, &tx, Step::Fail("the camera stopped (unplugged)"), &mut f);
        assert_eq!(e, Err(PlatformError::Failed("the camera stopped (unplugged)".into())));
        assert!(dropped.load(Ordering::Acquire), "the stream was closed");
    }

    #[test]
    fn frames_that_cannot_be_read_are_skipped_then_fail_the_capture() {
        let (tx, rx) = mpsc::channel();
        let mut c = CameraCapture::open_with(Arc::new(Fake::new(rx)), "", Some(GRAY)).unwrap();
        let mut f = VideoFrame::default();
        for _ in 0..BAD_FRAMES - 1 {
            tx.send(Step::Frame(vec![1], GRAY)).unwrap();
        }
        // A good frame after bad ones still arrives.
        grab_after(&mut c, &tx, Step::Frame(vec![5, 6], GRAY), &mut f).unwrap();
        for _ in 0..BAD_FRAMES - 1 {
            tx.send(Step::Frame(vec![1], GRAY)).unwrap();
        }
        let e = grab_after(&mut c, &tx, Step::Frame(vec![1], GRAY), &mut f);
        assert_eq!(e, Err(PlatformError::Failed("the camera sends frames that cannot be read".into())));
    }

    #[test]
    fn dropping_the_capture_stops_its_thread_after_the_current_frame() {
        let (tx, rx) = mpsc::channel();
        let fake = Arc::new(Fake::new(rx));
        let dropped = Arc::clone(&fake.dropped);
        let mut c = CameraCapture::open_with(fake.clone(), "", Some(GRAY)).unwrap();
        let mut f = VideoFrame::default();
        grab_after(&mut c, &tx, Step::Frame(vec![1, 2], GRAY), &mut f).unwrap();
        fake.until_waiting(2);
        drop(c);
        // The thread sees the stop flag once its current wait for a frame ends.
        tx.send(Step::Frame(vec![1, 2], GRAY)).unwrap();
        while !dropped.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_panic_in_the_camera_library_is_an_error() {
        assert_eq!(guarded::<()>(|| panic!("library bug")), Err(PlatformError::Failed("the camera stopped unexpectedly".into())));
    }

    /// A backend whose listing fails after the first read.
    struct Flaky(AtomicBool);

    impl CameraBackend for Flaky {
        fn cameras(&self) -> Result<Vec<CameraInfo>, PlatformError> {
            Ok(Vec::new())
        }
        fn formats(&self, _camera: &CameraInfo) -> Result<Vec<CameraFormat>, PlatformError> {
            if self.0.swap(true, Ordering::AcqRel) { Err(PlatformError::Failed("in use".into())) } else { Ok(vec![HD]) }
        }
        fn open(&self, _camera: &CameraInfo, _format: CameraFormat) -> Result<Box<dyn CameraStream>, PlatformError> {
            Err(PlatformError::Unsupported)
        }
    }

    #[test]
    fn a_camera_in_use_lists_the_formats_it_offered_before() {
        let flaky = Flaky(AtomicBool::new(false));
        let c = cam("Flaky camera", "formats-cache-test");
        assert_eq!(formats_with(&flaky, &c), vec![HD]);
        assert_eq!(formats_with(&flaky, &c), vec![HD], "the remembered list");
        assert_eq!(formats_with(&flaky, &cam("Never read", "x")), Vec::new());
    }

    #[test]
    fn a_stopped_capture_that_still_waits_on_its_camera_keeps_a_new_one_from_opening_it() {
        let stuck = cam("Stuck camera", "still-closing");
        let (tx, rx) = mpsc::channel();
        let first = Arc::new(Fake { list: vec![stuck.clone()], ..Fake::new(rx) });
        let mut c = CameraCapture::open_with(first.clone(), &stuck.key(), Some(GRAY)).unwrap();
        let mut f = VideoFrame::default();
        grab_after(&mut c, &tx, Step::Frame(vec![1, 2], GRAY), &mut f).unwrap();
        // Stopped while its thread waits for a frame that does not come.
        first.until_waiting(2);
        drop(c);
        // A stream it opened would end at once, so the test does not wait on it.
        let (_, rx2) = mpsc::channel::<Step>();
        let second = Fake { list: vec![stuck.clone()], ..Fake::new(rx2) };
        assert_eq!(failure(second, &stuck.key(), Some(GRAY)), PlatformError::Failed("the camera is still closing".into()));
        // Once the camera answers, the first thread sees the stop and ends; the camera opens again.
        let dropped = Arc::clone(&first.dropped);
        tx.send(Step::Frame(vec![1, 2], GRAY)).unwrap();
        while !dropped.load(Ordering::Acquire) || capturing().0.lock().unwrap().open.iter().any(|(k, _)| *k == stuck.key()) {
            std::thread::yield_now();
        }
        let (tx3, rx3) = mpsc::channel();
        let third = Arc::new(Fake { list: vec![stuck.clone()], ..Fake::new(rx3) });
        let mut c = CameraCapture::open_with(third, &stuck.key(), Some(GRAY)).unwrap();
        grab_after(&mut c, &tx3, Step::Frame(vec![1, 2], GRAY), &mut f).unwrap();
    }

    /// Grab until the capture gives something other than `Waiting` or `Unchanged`.
    fn grab_next(c: &mut CameraCapture, f: &mut VideoFrame) -> Result<(), PlatformError> {
        loop {
            match c.grab(f) {
                Err(PlatformError::Waiting) | Err(PlatformError::Unchanged) => std::thread::yield_now(),
                other => return other,
            }
        }
    }

    /// A camera `c` streams, and a second capture of it opened through a backend of its own,
    /// once that capture has joined the first. Returns the second backend too, to show it was
    /// never opened.
    fn second_capture(camera: &CameraInfo, format: Option<CameraFormat>) -> (CameraCapture, Arc<Fake>) {
        let (_, rx) = mpsc::channel::<Step>();
        let backend = Arc::new(Fake { list: vec![camera.clone()], ..Fake::new(rx) });
        let c = CameraCapture::open_with(backend.clone(), &camera.key(), format).unwrap();
        while c.reader.capture.get().is_none() {
            let error = c.reader.error.lock().unwrap().clone();
            assert!(error.is_none(), "{error:?}");
            std::thread::yield_now();
        }
        (c, backend)
    }

    #[test]
    fn two_captures_of_one_camera_get_the_same_frames() {
        let camera = cam("Shared camera", "two-readers");
        let (tx, rx) = mpsc::channel();
        let first = Arc::new(Fake { list: vec![camera.clone()], ..Fake::new(rx) });
        let mut a = CameraCapture::open_with(first.clone(), &camera.key(), Some(GRAY)).unwrap();
        let (mut fa, mut fb) = (VideoFrame::default(), VideoFrame::default());
        grab_after(&mut a, &tx, Step::Frame(vec![1, 2], GRAY), &mut fa).unwrap();
        let (mut b, second) = second_capture(&camera, Some(GRAY));
        assert_eq!(b.grab(&mut fb), Err(PlatformError::Waiting), "nothing before the next frame");
        for frame in [vec![3, 4], vec![5, 6]] {
            grab_after(&mut a, &tx, Step::Frame(frame.clone(), GRAY), &mut fa).unwrap();
            grab_next(&mut b, &mut fb).unwrap();
            let gray = [frame[0], frame[0], frame[0], 255, frame[1], frame[1], frame[1], 255];
            assert_eq!((fa.data.as_slice(), fb.data.as_slice()), (gray.as_slice(), gray.as_slice()), "both get the frame");
            assert_eq!((fb.width, fb.height, fb.format), (2, 1, PixelFormat::Rgba8));
        }
        assert_eq!(first.opened.lock().unwrap().len(), 1, "the camera was opened once");
        assert!(second.opened.lock().unwrap().is_empty(), "the second capture opened nothing");
        assert_eq!((a.other_format(), b.other_format()), (None, None));
    }

    #[test]
    fn the_last_capture_of_a_camera_closes_it() {
        let camera = cam("Shared camera", "last-reader");
        let (tx, rx) = mpsc::channel();
        let first = Arc::new(Fake { list: vec![camera.clone()], ..Fake::new(rx) });
        let dropped = Arc::clone(&first.dropped);
        let mut a = CameraCapture::open_with(first.clone(), &camera.key(), Some(GRAY)).unwrap();
        let mut f = VideoFrame::default();
        grab_after(&mut a, &tx, Step::Frame(vec![1, 2], GRAY), &mut f).unwrap();
        let (b, _second) = second_capture(&camera, Some(GRAY));
        drop(b);
        grab_after(&mut a, &tx, Step::Frame(vec![3, 4], GRAY), &mut f).unwrap();
        assert!(!dropped.load(Ordering::Acquire), "one capture still reads the camera");
        first.until_waiting(3);
        drop(a);
        tx.send(Step::Frame(vec![5, 6], GRAY)).unwrap();
        while !dropped.load(Ordering::Acquire) || capturing().0.lock().unwrap().open.iter().any(|(k, _)| *k == camera.key()) {
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_capture_dropped_while_its_camera_waits_for_a_frame_leaves_the_others_reading() {
        let camera = cam("Shared camera", "dropped-mid-frame");
        let (tx, rx) = mpsc::channel();
        let first = Arc::new(Fake { list: vec![camera.clone()], ..Fake::new(rx) });
        let mut a = CameraCapture::open_with(first.clone(), &camera.key(), Some(GRAY)).unwrap();
        let (mut fa, mut fb) = (VideoFrame::default(), VideoFrame::default());
        grab_after(&mut a, &tx, Step::Frame(vec![1, 2], GRAY), &mut fa).unwrap();
        let (mut b, _second) = second_capture(&camera, Some(GRAY));
        // The thread that opened the camera waits for a frame with both readers listed; the
        // capture it opened for is dropped meanwhile.
        first.until_waiting(2);
        drop(a);
        for frame in [vec![7, 8], vec![9, 10]] {
            tx.send(Step::Frame(frame.clone(), GRAY)).unwrap();
            grab_next(&mut b, &mut fb).unwrap();
            assert_eq!(fb.data[0], frame[0]);
        }
        assert!(!first.dropped.load(Ordering::Acquire), "the camera stays open for the other capture");
    }

    #[test]
    fn a_capture_that_asks_for_another_format_shares_the_one_the_camera_is_open_in() {
        let camera = cam("Shared camera", "format-conflict");
        let (tx, rx) = mpsc::channel();
        let first = Arc::new(Fake { list: vec![camera.clone()], ..Fake::new(rx) });
        let mut a = CameraCapture::open_with(first.clone(), &camera.key(), Some(GRAY)).unwrap();
        let (mut fa, mut fb) = (VideoFrame::default(), VideoFrame::default());
        grab_after(&mut a, &tx, Step::Frame(vec![1, 2], GRAY), &mut fa).unwrap();
        let (mut b, _second) = second_capture(&camera, Some(HD));
        let (c, _third) = second_capture(&camera, None);
        assert_eq!(b.other_format(), Some(GRAY), "the camera is open in another format");
        assert_eq!((a.other_format(), c.other_format()), (None, None), "the format it asked for, and no choice");
        grab_after(&mut a, &tx, Step::Frame(vec![3, 4], GRAY), &mut fa).unwrap();
        grab_next(&mut b, &mut fb).unwrap();
        assert_eq!((fb.width, fb.height), (2, 1), "frames come in the open format");
        assert_eq!(first.opened.lock().unwrap().as_slice(), &[(camera, GRAY)]);
    }

    #[test]
    fn a_failed_camera_fails_every_capture_and_opens_again_for_both() {
        let camera = cam("Shared camera", "fails-both");
        let (tx, rx) = mpsc::channel();
        let first = Arc::new(Fake { list: vec![camera.clone()], ..Fake::new(rx) });
        let mut a = CameraCapture::open_with(first, &camera.key(), Some(GRAY)).unwrap();
        let (mut fa, mut fb) = (VideoFrame::default(), VideoFrame::default());
        grab_after(&mut a, &tx, Step::Frame(vec![1, 2], GRAY), &mut fa).unwrap();
        let (mut b, _second) = second_capture(&camera, Some(GRAY));
        let unplugged = Err(PlatformError::Failed("the camera stopped (unplugged)".into()));
        assert_eq!(grab_after(&mut a, &tx, Step::Fail("the camera stopped (unplugged)"), &mut fa), unplugged);
        assert_eq!(grab_next(&mut b, &mut fb), unplugged, "the other capture is told too");
        drop((a, b));
        // Each source opens its camera again: the first to do so captures it, and the other joins.
        let (tx2, rx2) = mpsc::channel();
        let again = Arc::new(Fake { list: vec![camera.clone()], ..Fake::new(rx2) });
        let mut a = CameraCapture::open_with(again.clone(), &camera.key(), Some(GRAY)).unwrap();
        grab_after(&mut a, &tx2, Step::Frame(vec![1, 2], GRAY), &mut fa).unwrap();
        let (mut b, _) = second_capture(&camera, Some(GRAY));
        grab_after(&mut a, &tx2, Step::Frame(vec![3, 4], GRAY), &mut fa).unwrap();
        grab_next(&mut b, &mut fb).unwrap();
        assert_eq!((fa.data[0], fb.data[0]), (3, 3));
        assert_eq!(again.opened.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_capture_opened_while_the_camera_opens_waits_and_takes_over_when_that_open_fails() {
        let camera = cam("Shared camera", "open-fails-for-the-first");
        let (_tx, rx) = mpsc::channel::<Step>();
        let (release, held) = mpsc::channel();
        let first = Arc::new(Fake { list: vec![camera.clone()], formats_gate: Mutex::new(Some(held)), ..Fake::new(rx) });
        let odd = CameraFormat { width: 999, height: 9, fps: 7, pixel: CameraPixel::Rgb };
        // The first capture asks for a format the camera does not offer, and its format read waits.
        let mut a = CameraCapture::open_with(first.clone(), &camera.key(), Some(odd)).unwrap();
        while first.reads.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        let (tx2, rx2) = mpsc::channel();
        let second = Arc::new(Fake { list: vec![camera.clone()], ..Fake::new(rx2) });
        let mut b = CameraCapture::open_with(second.clone(), &camera.key(), None).unwrap();
        while !b.reader.waited.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        release.send(()).unwrap();
        let mut f = VideoFrame::default();
        assert_eq!(grab_next(&mut a, &mut f), Err(PlatformError::Failed("the camera does not offer 999 x 9, 7 fps, RGB".into())));
        grab_after(&mut b, &tx2, Step::Frame(vec![1, 2], GRAY), &mut f).unwrap();
        assert_eq!(second.opened.lock().unwrap().as_slice(), &[(camera, HD)], "the second capture opened it at the default format");
    }

    #[test]
    fn a_capture_waiting_for_an_opening_camera_takes_over_when_the_opening_capture_is_dropped() {
        let odd = CameraFormat { width: 999, height: 9, fps: 7, pixel: CameraPixel::Rgb };
        // The first capture's format check fails, or passes; either way it is dropped first.
        for (id, asked) in [("dropped-opener-fails", odd), ("dropped-opener-fits", GRAY)] {
            let camera = cam("Shared camera", id);
            let (_tx, rx) = mpsc::channel::<Step>();
            let (release, held) = mpsc::channel();
            let first = Arc::new(Fake { list: vec![camera.clone()], formats_gate: Mutex::new(Some(held)), ..Fake::new(rx) });
            let a = CameraCapture::open_with(first.clone(), &camera.key(), Some(asked)).unwrap();
            while first.reads.load(Ordering::Acquire) == 0 {
                std::thread::yield_now();
            }
            let (tx2, rx2) = mpsc::channel();
            let second = Arc::new(Fake { list: vec![camera.clone()], ..Fake::new(rx2) });
            let mut b = CameraCapture::open_with(second.clone(), &camera.key(), None).unwrap();
            while !b.reader.waited.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            drop(a);
            release.send(()).unwrap();
            let mut f = VideoFrame::default();
            assert_eq!(grab_after(&mut b, &tx2, Step::Frame(vec![1, 2], GRAY), &mut f), Ok(()), "{id}");
            assert!(first.opened.lock().unwrap().is_empty(), "{id}: the dropped capture did not open the camera");
            assert_eq!(second.opened.lock().unwrap().as_slice(), &[(camera, HD)], "{id}: the waiting capture opened it");
        }
    }

    #[test]
    fn a_camera_another_capture_streams_stays_listed() {
        let camera = cam("Streamed camera", "listed-while-streaming");
        let idle = cam("Idle camera", "listed-when-it-answers");
        let (tx, rx) = mpsc::channel();
        let fake = Arc::new(Fake { list: vec![camera.clone()], ..Fake::new(rx) });
        let mut c = CameraCapture::open_with(fake, &camera.key(), Some(GRAY)).unwrap();
        let mut f = VideoFrame::default();
        grab_after(&mut c, &tx, Step::Frame(vec![1, 2], GRAY), &mut f).unwrap();
        // The driver refuses to open either camera; the streamed one is not asked.
        let listed = vec![(camera.clone(), false), (idle.clone(), false)];
        assert_eq!(sending(listed, |answers| *answers), vec![camera.clone()]);
        assert_eq!(sending(vec![(idle.clone(), true)], |answers| *answers), vec![idle], "a camera that answers is listed");
    }

    /// A camera, `slow`, whose format read waits until the test lets it finish; other cameras
    /// are read at once.
    struct SlowFormats {
        slow: CameraInfo,
        entered: Mutex<mpsc::Sender<()>>,
        finish: Mutex<mpsc::Receiver<()>>,
        reads: std::sync::atomic::AtomicUsize,
    }

    impl CameraBackend for SlowFormats {
        fn cameras(&self) -> Result<Vec<CameraInfo>, PlatformError> {
            Ok(vec![self.slow.clone()])
        }
        fn formats(&self, camera: &CameraInfo) -> Result<Vec<CameraFormat>, PlatformError> {
            self.reads.fetch_add(1, Ordering::AcqRel);
            if *camera == self.slow {
                self.entered.lock().unwrap().send(()).unwrap();
                self.finish.lock().unwrap().recv().unwrap();
            }
            Ok(vec![HD])
        }
        fn open(&self, _camera: &CameraInfo, _format: CameraFormat) -> Result<Box<dyn CameraStream>, PlatformError> {
            Err(PlatformError::Unsupported)
        }
    }

    #[test]
    fn a_camera_slow_to_give_its_formats_holds_up_no_other_read() {
        let slow = cam("Slow camera", "slow-format-read");
        let other = cam("Other camera", "beside-a-slow-format-read");
        let (entered_tx, entered) = mpsc::channel();
        let (finish, finish_rx) = mpsc::channel();
        let backend = Arc::new(SlowFormats {
            slow: slow.clone(),
            entered: Mutex::new(entered_tx),
            finish: Mutex::new(finish_rx),
            reads: std::sync::atomic::AtomicUsize::new(0),
        });
        finish.send(()).unwrap();
        assert_eq!(formats_with(backend.as_ref(), &slow), vec![HD]);
        entered.recv().unwrap();
        let reading = {
            let (backend, slow) = (Arc::clone(&backend), slow.clone());
            std::thread::spawn(move || formats_with(backend.as_ref(), &slow))
        };
        entered.recv().unwrap();
        // While that read waits on the slow camera:
        assert_eq!(formats_with(backend.as_ref(), &other), vec![HD], "another camera is read");
        let reads = backend.reads.load(Ordering::Acquire);
        assert_eq!(formats_with(backend.as_ref(), &slow), vec![HD], "the slow camera gives the formats it offered last time");
        assert_eq!(backend.reads.load(Ordering::Acquire), reads, "without being read twice at once");
        finish.send(()).unwrap();
        assert_eq!(reading.join().unwrap(), vec![HD]);
    }

    #[test]
    fn a_camera_being_captured_is_not_opened_to_read_its_formats() {
        let (tx, rx) = mpsc::channel();
        let busy = cam("Captured camera", "formats-while-capturing");
        let fake = Arc::new(Fake { list: vec![busy.clone()], ..Fake::new(rx) });
        let mut c = CameraCapture::open_with(fake.clone(), &busy.key(), Some(GRAY)).unwrap();
        let mut f = VideoFrame::default();
        grab_after(&mut c, &tx, Step::Frame(vec![1, 2], GRAY), &mut f).unwrap();
        let reads = fake.reads.load(Ordering::Acquire);
        assert_eq!(formats_with(fake.as_ref(), &busy), vec![HD, GRAY], "the formats read when the capture opened it");
        assert_eq!(fake.reads.load(Ordering::Acquire), reads, "the camera was not opened again");
        // Once the capture ends, the camera is read again.
        let dropped = Arc::clone(&fake.dropped);
        drop(c);
        drop(tx);
        while !dropped.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        while capturing().0.lock().unwrap().open.iter().any(|(k, _)| *k == busy.key()) {
            std::thread::yield_now();
        }
        formats_with(fake.as_ref(), &busy);
        assert_eq!(fake.reads.load(Ordering::Acquire), reads + 1);
    }

    #[test]
    fn listing_and_opening_the_system_cameras_does_not_fail() {
        // Build machines have no camera: the list is empty and opening one reports an error.
        let list = cameras();
        for c in &list {
            assert!(!c.name.is_empty());
        }
        if list.is_empty() {
            let mut cap = CameraCapture::open("", None).unwrap();
            let mut f = VideoFrame::default();
            loop {
                match cap.grab(&mut f) {
                    Err(PlatformError::Waiting) => std::thread::yield_now(),
                    other => {
                        assert!(other.is_err());
                        break;
                    }
                }
            }
        }
    }
}
