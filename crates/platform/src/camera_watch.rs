//! Notices of cameras plugged in or out.
//!
//! A [`Watch`] has a thread of its own that waits for the system to report a change in its
//! cameras: a device watcher for video capture devices on Windows, and the V4L2 nodes appearing in
//! and leaving `/dev` on Linux. After each notice the thread lists the cameras again and, when the
//! list changed, publishes it with a new generation. Readers take the generation from an atomic,
//! so a source thread can look at it every frame, and take the list when the generation changed.
//! Taking the list does not wait for the watch's thread.
//! macOS and the other systems have no watch: there the cameras are found by listing them again.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use arc_swap::ArcSwapOption;

use crate::PlatformError;
use crate::camera::{CameraBackend, CameraInfo};

/// The system's notices that its cameras may have changed. Created and used on the watch's thread.
pub trait CameraEvents {
    /// Wait for the next notice. An error ends the watch.
    fn wait(&mut self) -> Result<(), PlatformError>;

    /// Whether the notices the last wait gave can include a camera unplugged. A camera unplugged
    /// and plugged in again before the cameras are listed leaves the list as it was, so this is
    /// the only sign of it.
    fn unplugged(&mut self) -> bool {
        false
    }
}

/// The cameras as the system last reported them; see the module documentation.
pub struct Watch {
    /// Changes each time `list` does.
    generation: AtomicU64,
    /// The cameras; `None` until they are first listed, and after a listing failed.
    list: ArcSwapOption<Vec<CameraInfo>>,
    /// Counts the notices the thread has handled, a listing done for each.
    handled: AtomicU64,
    /// Counts the handled notices that can include a camera unplugged.
    unplugs: AtomicU64,
    /// Cleared when the notices stop or could not be set up.
    live: AtomicBool,
}

impl Watch {
    /// A value that changes when the listed cameras change, and when the notices stop.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// The cameras the system lists: `None` while they have not been listed yet, while a listing
    /// failed, and once the notices stop. This does not wait on the system or on the watch's
    /// thread.
    pub fn cameras(&self) -> Option<Arc<Vec<CameraInfo>>> {
        if !self.live() {
            return None;
        }
        self.list.load_full()
    }

    /// Whether notices arrive.
    pub fn live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }

    /// How many notices the thread has handled.
    pub fn handled(&self) -> u64 {
        self.handled.load(Ordering::Acquire)
    }

    /// A value that changes after each notice that can include a camera unplugged, also when
    /// the list stays the same.
    pub fn unplugs(&self) -> u64 {
        self.unplugs.load(Ordering::Acquire)
    }

    /// Publish `listed`, with a new generation when it differs from the list before. Only the
    /// watch's thread publishes.
    fn publish(&self, listed: Option<Vec<CameraInfo>>) {
        if self.list.load().as_deref() != listed.as_ref() {
            self.list.store(listed.map(Arc::new));
            self.generation.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// Watch the cameras of `backend` with the notices `events` sets up, on a thread of its own. The
/// thread ends when the notices stop or the watch is dropped. When no thread can be started the
/// watch is not live.
pub fn watch_with<F>(backend: Arc<dyn CameraBackend>, events: F) -> Arc<Watch>
where
    F: FnOnce() -> Result<Box<dyn CameraEvents>, PlatformError> + Send + 'static,
{
    let watch = Arc::new(Watch {
        generation: AtomicU64::new(0),
        list: ArcSwapOption::empty(),
        handled: AtomicU64::new(0),
        unplugs: AtomicU64::new(0),
        live: AtomicBool::new(true),
    });
    let weak = Arc::downgrade(&watch);
    let started = std::thread::Builder::new().name("camera watch".into()).spawn(move || {
        let ended = Ended(weak.clone());
        let Ok(mut events) = events() else { return };
        // The list taken after the notices are set up, so a camera plugged in meanwhile is in it.
        let Some(watch) = weak.upgrade() else { return };
        watch.publish(backend.cameras().ok());
        drop(watch);
        while events.wait().is_ok() {
            let unplugged = events.unplugged();
            let Some(watch) = weak.upgrade() else { return };
            watch.publish(backend.cameras().ok());
            if unplugged {
                watch.unplugs.fetch_add(1, Ordering::AcqRel);
            }
            watch.handled.fetch_add(1, Ordering::AcqRel);
        }
        drop(ended);
    });
    if started.is_err() {
        watch.live.store(false, Ordering::Release);
    }
    watch
}

/// Marks the watch as not live, with a new generation, when its thread ends, also by a panic in
/// the system's library.
struct Ended(Weak<Watch>);

impl Drop for Ended {
    fn drop(&mut self) {
        if let Some(watch) = self.0.upgrade() {
            // A reader that sees the new generation finds no list.
            watch.list.store(None);
            watch.generation.fetch_add(1, Ordering::AcqRel);
            watch.live.store(false, Ordering::Release);
        }
    }
}

/// The watch of the system's cameras, started on the first call; `None` where the system sends no
/// notices of cameras plugged in or out.
pub fn watch() -> Option<Arc<Watch>> {
    static WATCH: OnceLock<Option<Arc<Watch>>> = OnceLock::new();
    WATCH.get_or_init(|| imp::SENDS_NOTICES.then(|| watch_with(crate::camera::system(), imp::events))).clone()
}

/// A device watcher for the video capture device class, through the Windows Runtime.
#[cfg(windows)]
mod imp {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::time::Duration;

    use windows::Devices::Enumeration::{DeviceClass, DeviceInformation, DeviceInformationUpdate, DeviceWatcher, DeviceWatcherStatus};
    use windows::Foundation::TypedEventHandler;
    use windows::core::IInspectable;

    use super::CameraEvents;
    use crate::PlatformError;

    pub(super) const SENDS_NOTICES: bool = true;

    fn cannot(e: impl std::fmt::Display) -> PlatformError {
        PlatformError::Failed(format!("camera notices could not be set up ({e})"))
    }

    /// How often a watch with no notices asks whether the watcher still runs, so a watcher that
    /// aborted without an event is noticed.
    const STATUS: Duration = Duration::from_secs(10);

    struct Devices {
        watcher: DeviceWatcher,
        /// Wakes the watch's thread. It holds one wake-up: the events that arrive while one
        /// waits are handled by the listing it starts.
        woken: mpsc::Receiver<()>,
        /// Set when the watcher stopped and sends nothing more.
        stopped: Arc<AtomicBool>,
        /// Set when the watcher reported a device removed, until the watch's thread takes it.
        removed: Arc<AtomicBool>,
        /// The thread's COM apartment, kept while the watcher runs.
        _com: comedy::com::ComApartmentScope,
    }

    /// An event handler that wakes the watch's thread, after setting the flag `stop` when given.
    fn waker(wake: &mpsc::SyncSender<()>, stop: Option<&Arc<AtomicBool>>) -> impl Fn() + Send + 'static {
        let (wake, stop) = (wake.clone(), stop.cloned());
        move || {
            if let Some(stop) = &stop {
                stop.store(true, Ordering::Release);
            }
            let _ = wake.try_send(());
        }
    }

    pub(super) fn events() -> Result<Box<dyn CameraEvents>, PlatformError> {
        let com = comedy::com::ComApartmentScope::init_mta().map_err(cannot)?;
        let watcher = DeviceInformation::CreateWatcherDeviceClass(DeviceClass::VideoCapture).map_err(cannot)?;
        let (wake, woken) = mpsc::sync_channel(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let gone = Arc::new(AtomicBool::new(false));
        let (added, removed, updated, ended) = (waker(&wake, None), waker(&wake, Some(&gone)), waker(&wake, None), waker(&wake, Some(&stopped)));
        // The watcher reports devices that were added or removed only to an app that handles
        // all three events.
        watcher
            .Added(&TypedEventHandler::<DeviceWatcher, DeviceInformation>::new(move |_, _| {
                added();
                Ok(())
            }))
            .map_err(cannot)?;
        watcher
            .Removed(&TypedEventHandler::<DeviceWatcher, DeviceInformationUpdate>::new(move |_, _| {
                removed();
                Ok(())
            }))
            .map_err(cannot)?;
        watcher
            .Updated(&TypedEventHandler::<DeviceWatcher, DeviceInformationUpdate>::new(move |_, _| {
                updated();
                Ok(())
            }))
            .map_err(cannot)?;
        watcher
            .Stopped(&TypedEventHandler::<DeviceWatcher, IInspectable>::new(move |_, _| {
                ended();
                Ok(())
            }))
            .map_err(cannot)?;
        watcher.Start().map_err(cannot)?;
        Ok(Box::new(Devices { watcher, woken, stopped, removed: gone, _com: com }))
    }

    impl Devices {
        /// Whether the watcher has stopped or aborted.
        fn ended(&self) -> bool {
            self.stopped.load(Ordering::Acquire)
                || self.watcher.Status().map_or(true, |s| s == DeviceWatcherStatus::Stopped || s == DeviceWatcherStatus::Aborted)
        }
    }

    impl CameraEvents for Devices {
        /// The events since the last wait are handled as one. An error once the watcher stopped
        /// or aborted.
        fn wait(&mut self) -> Result<(), PlatformError> {
            loop {
                match self.woken.recv_timeout(STATUS) {
                    Ok(()) if !self.stopped.load(Ordering::Acquire) => return Ok(()),
                    Err(RecvTimeoutError::Timeout) if !self.ended() => {}
                    _ => return Err(PlatformError::Failed("camera notices stopped".into())),
                }
            }
        }

        /// The removal is flagged before its wake-up is sent, so a wait that a removal ended
        /// finds it.
        fn unplugged(&mut self) -> bool {
            self.removed.swap(false, Ordering::AcqRel)
        }
    }

    impl Drop for Devices {
        fn drop(&mut self) {
            let _ = self.watcher.Stop();
        }
    }
}

/// The V4L2 nodes (`/dev/video…`) created, removed or given new permissions, through inotify.
#[cfg(target_os = "linux")]
mod imp {
    use std::mem::MaybeUninit;
    use std::os::fd::OwnedFd;

    use rustix::fs::inotify::{self, CreateFlags, ReadFlags, WatchFlags};
    use rustix::io::Errno;

    use super::CameraEvents;
    use crate::PlatformError;

    pub(super) const SENDS_NOTICES: bool = true;

    /// The bytes read from inotify at once: room for about 100 notices.
    const READ: usize = 4096;

    struct Nodes {
        fd: OwnedFd,
        buf: Vec<MaybeUninit<u8>>,
        /// Whether the last read had a notice that can mean a camera was unplugged.
        removed: bool,
    }

    pub(super) fn events() -> Result<Box<dyn CameraEvents>, PlatformError> {
        let cannot = |e: Errno| PlatformError::Failed(format!("camera notices could not be set up ({e})"));
        let fd = inotify::init(CreateFlags::CLOEXEC).map_err(cannot)?;
        // Udev gives a new node its permissions after the node appears, and a camera that can
        // be opened is listed only then.
        inotify::add_watch(&fd, "/dev", WatchFlags::CREATE | WatchFlags::DELETE | WatchFlags::ATTRIB).map_err(cannot)?;
        Ok(Box::new(Nodes { fd, buf: vec![MaybeUninit::uninit(); READ], removed: false }))
    }

    /// Whether the notice `flags` for the file `name` can mean a camera was unplugged. Notices
    /// lost to a full queue can.
    pub(super) fn about_an_unplug(flags: ReadFlags, name: Option<&[u8]>) -> bool {
        flags.contains(ReadFlags::QUEUE_OVERFLOW) || (flags.contains(ReadFlags::DELETE) && about_a_camera(flags, name))
    }

    /// Whether the notice `flags` for the file `name` can mean a camera was plugged in or out.
    pub(super) fn about_a_camera(flags: ReadFlags, name: Option<&[u8]>) -> bool {
        flags.contains(ReadFlags::QUEUE_OVERFLOW) || name.is_some_and(|n| n.starts_with(b"video"))
    }

    /// Whether the notice `flags` mean the watch on `/dev` was removed, as when `/dev` is
    /// unmounted. No notice follows one.
    pub(super) fn watch_ended(flags: ReadFlags) -> bool {
        flags.intersects(ReadFlags::IGNORED | ReadFlags::UNMOUNT)
    }

    impl CameraEvents for Nodes {
        /// Every notice of one read is taken, and the read gives one notice when any of them is
        /// about a camera. The cameras are listed after the read, so they include every change
        /// it reported. An error once the watch on `/dev` was removed.
        fn wait(&mut self) -> Result<(), PlatformError> {
            let mut reader = inotify::Reader::new(&self.fd, &mut self.buf);
            let mut camera = false;
            self.removed = false;
            loop {
                match reader.next() {
                    Ok(e) if watch_ended(e.events()) => {
                        return Err(PlatformError::Failed("camera notices stopped (/dev is no longer watched)".into()));
                    }
                    Ok(e) => {
                        let name = e.file_name().map(|n| n.to_bytes());
                        camera |= about_a_camera(e.events(), name);
                        self.removed |= about_an_unplug(e.events(), name);
                    }
                    Err(e) if e == Errno::INTR => {}
                    Err(e) => return Err(PlatformError::Failed(format!("camera notices stopped ({e})"))),
                }
                if camera && reader.is_buffer_empty() {
                    return Ok(());
                }
            }
        }

        fn unplugged(&mut self) -> bool {
            self.removed
        }
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use super::CameraEvents;
    use crate::PlatformError;

    pub(super) const SENDS_NOTICES: bool = false;

    pub(super) fn events() -> Result<Box<dyn CameraEvents>, PlatformError> {
        Err(PlatformError::Unsupported)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, mpsc};

    use super::*;
    use crate::camera::{CameraFormat, CameraStream};

    fn cam(name: &str, id: &str) -> CameraInfo {
        CameraInfo { name: name.into(), id: id.into() }
    }

    /// Cameras a test plugs in and out.
    struct Plugs(Mutex<Result<Vec<CameraInfo>, PlatformError>>);

    impl Plugs {
        fn new(list: Vec<CameraInfo>) -> Arc<Plugs> {
            Arc::new(Plugs(Mutex::new(Ok(list))))
        }

        fn set(&self, list: Result<Vec<CameraInfo>, PlatformError>) {
            *self.0.lock().unwrap() = list;
        }
    }

    impl CameraBackend for Plugs {
        fn cameras(&self) -> Result<Vec<CameraInfo>, PlatformError> {
            self.0.lock().unwrap().clone()
        }
        fn formats(&self, _camera: &CameraInfo) -> Result<Vec<CameraFormat>, PlatformError> {
            Err(PlatformError::Unsupported)
        }
        fn open(&self, _camera: &CameraInfo, _format: CameraFormat) -> Result<Box<dyn CameraStream>, PlatformError> {
            Err(PlatformError::Unsupported)
        }
    }

    /// Notices a test sends, each saying whether it can include a camera unplugged; dropping the
    /// sender ends them.
    struct Told(mpsc::Receiver<bool>, bool);

    impl CameraEvents for Told {
        fn wait(&mut self) -> Result<(), PlatformError> {
            self.1 = self.0.recv().map_err(|_| PlatformError::Failed("stopped".into()))?;
            Ok(())
        }
        fn unplugged(&mut self) -> bool {
            self.1
        }
    }

    fn watched(plugs: &Arc<Plugs>) -> (Arc<Watch>, mpsc::Sender<bool>) {
        let (tell, told) = mpsc::channel();
        let backend: Arc<dyn CameraBackend> = plugs.clone();
        let w = watch_with(backend, move || Ok(Box::new(Told(told, false)) as Box<dyn CameraEvents>));
        while w.generation() == 0 && w.live() {
            std::thread::yield_now();
        }
        (w, tell)
    }

    /// Send a notice and wait until the watch has handled it.
    fn notice(w: &Watch, tell: &mpsc::Sender<bool>) {
        told(w, tell, false);
    }

    /// Send a notice that can include a camera unplugged, or not, and wait until the watch has
    /// handled it.
    fn told(w: &Watch, tell: &mpsc::Sender<bool>, unplugged: bool) {
        let before = w.handled();
        tell.send(unplugged).unwrap();
        while w.handled() == before {
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_camera_unplugged_and_plugged_in_again_is_counted_while_the_list_stays() {
        let plugs = Plugs::new(vec![cam("Desk", "1")]);
        let (w, tell) = watched(&plugs);
        let (listed, unplugs) = (w.generation(), w.unplugs());
        notice(&w, &tell);
        assert_eq!(w.unplugs(), unplugs, "a notice of a camera plugged in is not counted");
        told(&w, &tell, true);
        assert_eq!(w.generation(), listed, "the list stays the same");
        assert_ne!(w.unplugs(), unplugs, "the camera may have been unplugged and plugged in again");
    }

    #[test]
    fn a_camera_plugged_in_or_out_is_listed_with_a_new_generation() {
        let plugs = Plugs::new(vec![cam("Desk", "1")]);
        let (w, tell) = watched(&plugs);
        let first = w.generation();
        assert_eq!(w.cameras().as_deref(), Some(&vec![cam("Desk", "1")]), "listed once the notices are set up");
        plugs.set(Ok(vec![cam("Desk", "1"), cam("Room", "2")]));
        notice(&w, &tell);
        assert_ne!(w.generation(), first, "plugged in");
        assert_eq!(w.cameras().map(|l| l.len()), Some(2));
        let both = w.generation();
        notice(&w, &tell);
        assert_eq!(w.generation(), both, "a notice that changes nothing keeps the generation");
        plugs.set(Ok(vec![cam("Room", "2")]));
        notice(&w, &tell);
        assert_ne!(w.generation(), both, "unplugged");
        assert_eq!(w.cameras().as_deref(), Some(&vec![cam("Room", "2")]));
    }

    #[test]
    fn a_failed_listing_makes_the_list_unknown_until_it_works_again() {
        let plugs = Plugs::new(Vec::new());
        let (w, tell) = watched(&plugs);
        assert_eq!(w.cameras().as_deref(), Some(&Vec::new()), "no camera is a known list");
        plugs.set(Err(PlatformError::Failed("the cameras could not be listed".into())));
        notice(&w, &tell);
        assert_eq!(w.cameras(), None);
        plugs.set(Ok(vec![cam("Desk", "1")]));
        notice(&w, &tell);
        assert_eq!(w.cameras().map(|l| l.len()), Some(1));
    }

    #[test]
    fn the_watch_stops_being_live_when_its_notices_stop_or_cannot_start() {
        let plugs = Plugs::new(vec![cam("Desk", "1")]);
        let (w, tell) = watched(&plugs);
        assert!(w.live());
        let listed = w.generation();
        drop(tell);
        while w.live() {
            std::thread::yield_now();
        }
        assert_eq!(w.cameras(), None, "a list that is no longer kept is not given");
        assert_ne!(w.generation(), listed, "readers see that the list is no longer kept");
        let backend: Arc<dyn CameraBackend> = plugs;
        let never = watch_with(backend, || Err(PlatformError::Unsupported));
        while never.live() {
            std::thread::yield_now();
        }
        assert_eq!(never.cameras(), None);
    }

    /// Notices that count the thread's waits and record when the thread dropped them.
    struct Counted {
        told: mpsc::Receiver<()>,
        waits: Arc<AtomicU64>,
        dropped: Arc<AtomicBool>,
    }

    impl CameraEvents for Counted {
        fn wait(&mut self) -> Result<(), PlatformError> {
            self.waits.fetch_add(1, Ordering::AcqRel);
            self.told.recv().map_err(|_| PlatformError::Failed("stopped".into()))
        }
    }

    impl Drop for Counted {
        fn drop(&mut self) {
            self.dropped.store(true, Ordering::Release);
        }
    }

    #[test]
    fn a_dropped_watch_ends_its_thread_at_the_next_notice() {
        let plugs = Plugs::new(Vec::new());
        let (tell, told) = mpsc::channel();
        let waits = Arc::new(AtomicU64::new(0));
        let dropped = Arc::new(AtomicBool::new(false));
        let events = Counted { told, waits: waits.clone(), dropped: dropped.clone() };
        let backend: Arc<dyn CameraBackend> = plugs.clone();
        let w = watch_with(backend, move || Ok(Box::new(events) as Box<dyn CameraEvents>));
        while waits.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        drop(w);
        tell.send(()).unwrap();
        // The thread holds the watch only while it lists, so at the notice it either ends, which
        // drops its notices, or waits for another notice.
        while !dropped.load(Ordering::Acquire) && waits.load(Ordering::Acquire) < 2 {
            std::thread::yield_now();
        }
        assert!(dropped.load(Ordering::Acquire), "the thread waits for another notice instead of ending");
        assert_eq!(waits.load(Ordering::Acquire), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_notices_about_video_nodes_wake_the_watch() {
        use rustix::fs::inotify::ReadFlags;
        assert!(imp::about_a_camera(ReadFlags::CREATE, Some(b"video0")));
        assert!(imp::about_a_camera(ReadFlags::DELETE, Some(b"video12")));
        assert!(imp::about_a_camera(ReadFlags::ATTRIB, Some(b"video2")));
        assert!(!imp::about_a_camera(ReadFlags::CREATE, Some(b"ttyUSB0")));
        assert!(!imp::about_a_camera(ReadFlags::CREATE, None));
        assert!(imp::about_a_camera(ReadFlags::QUEUE_OVERFLOW, None), "notices were lost: list again");
        assert!(imp::watch_ended(ReadFlags::IGNORED), "the watch on /dev was removed");
        assert!(imp::watch_ended(ReadFlags::UNMOUNT | ReadFlags::IGNORED));
        assert!(!imp::watch_ended(ReadFlags::DELETE));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn only_a_removed_video_node_or_lost_notices_count_as_an_unplug() {
        use rustix::fs::inotify::ReadFlags;
        assert!(imp::about_an_unplug(ReadFlags::DELETE, Some(b"video0")));
        assert!(imp::about_an_unplug(ReadFlags::QUEUE_OVERFLOW, None), "notices were lost");
        assert!(!imp::about_an_unplug(ReadFlags::CREATE, Some(b"video0")));
        assert!(!imp::about_an_unplug(ReadFlags::ATTRIB, Some(b"video0")));
        assert!(!imp::about_an_unplug(ReadFlags::DELETE, Some(b"ttyUSB0")));
    }
}
