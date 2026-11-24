//! Audio devices: listing, input capture, capture of what an output device plays (Windows and
//! Linux), and playback on an output device (Windows and macOS).
//!
//! Windows and macOS use cpal. Linux uses the PipeWire sound server: capture asks for 48 kHz
//! stereo, and PipeWire converts each device's own format to it.

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, OnceLock};

use crate::PlatformError;

/// Which side of the sound system a device is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    /// Microphones and line inputs.
    Input,
    /// Speakers and headphones; capturing one records what it plays.
    Output,
}

/// The id stored in settings for the system's default device.
pub const DEFAULT_DEVICE: &str = "default";

/// A device the system reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    /// Stored in settings (the device name, which the system keeps across restarts).
    pub id: String,
    pub name: String,
    pub default: bool,
}

/// The layout of the samples a capture delivers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub rate: u32,
    pub channels: u16,
}

/// Devices of `kind`, the default first.
pub fn devices(kind: DeviceKind) -> Vec<DeviceInfo> {
    let mut list = imp::devices(kind);
    list.sort_by_key(|d| !d.default);
    list
}

/// A running capture; dropping it stops the device stream. It must stay on the thread that
/// opened it.
pub struct AudioCapture {
    _inner: imp::Capture,
    pub format: Format,
}

impl AudioCapture {
    /// Capture the device `id` of `kind` ([`DEFAULT_DEVICE`] or empty for the default).
    /// `on_data` receives interleaved samples on the device's own thread.
    pub fn open(
        kind: DeviceKind,
        id: &str,
        on_data: impl FnMut(&[f32], Format) + Send + 'static,
        on_error: impl FnMut(String) + Send + 'static,
    ) -> Result<AudioCapture, PlatformError> {
        let (inner, format) = imp::Capture::open(kind, id, on_data, on_error)?;
        Ok(AudioCapture { _inner: inner, format })
    }
}

/// Fills each buffer a playing device asks for with interleaved samples in the device's format.
/// It runs on the device's own thread, so it must not block or allocate.
pub type Fill = Box<dyn FnMut(&mut [f32], Format) + Send>;

/// Hears why a playing device stopped (it failed or was unplugged), on the device's thread.
pub type OnError = Box<dyn FnMut(String) + Send>;

/// Plays audio on output devices: [`SystemSink`] on the system's devices, or a stand-in in tests.
pub trait AudioSink: Send + Sync {
    /// Start playing on the output device `id` ([`DEFAULT_DEVICE`] or empty for the default).
    /// Playback runs until the returned handle is dropped. The device may still be opening when
    /// this returns; [`Playing::started`] and [`Playing::open_error`] say when it has started or
    /// why it could not.
    fn play(&self, id: &str, fill: Fill, on_error: OnError) -> Result<Box<dyn Playing>, PlatformError>;
}

/// An output device that is playing or opening; dropping it stops the device.
pub trait Playing: Send {
    /// The device's sample rate and channel count, once it has started.
    fn format(&self) -> Format;

    /// Whether the device has started. A handle that is playing when it is returned keeps the
    /// default.
    fn started(&self) -> bool {
        true
    }

    /// Why the device could not start, once its open has failed.
    fn open_error(&self) -> Option<&str> {
        None
    }
}

/// The system's output devices.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemSink;

impl AudioSink for SystemSink {
    fn play(&self, id: &str, fill: Fill, on_error: OnError) -> Result<Box<dyn Playing>, PlatformError> {
        Ok(Box::new(AudioPlayback::open(id, fill, on_error)?))
    }
}

/// Requests the device thread holds at most. While it is that far behind (a device that takes
/// long to open), [`AudioPlayback::open`] reports the device as busy.
const REQUESTS_QUEUED: usize = 8;

/// Opens a device stream on the device thread.
type OpenStream = Box<dyn FnOnce(Fill, OnError) -> Result<(Box<dyn Any>, Format), PlatformError> + Send>;

enum Request {
    Open {
        open: OpenStream,
        fill: Fill,
        on_error: OnError,
        slot: Pending,
    },
    /// A handle was dropped; its stream closes.
    Close,
}

/// The device thread's side of a [`Slot`]. Dropped before the open has ended, as when the device
/// thread stops, it tells the handle that the device could not open.
struct Pending(Arc<Slot>);

impl Drop for Pending {
    fn drop(&mut self) {
        let _ = self.0.started.set(Err("the audio device thread stopped".into()));
    }
}

/// What a playback's handle and the device thread share.
#[derive(Default)]
struct Slot {
    /// Set once by the device thread: the device's format, or why it could not open.
    started: OnceLock<Result<Format, String>>,
    /// Set when the handle is dropped.
    dropped: AtomicBool,
}

/// The thread that opens, holds and closes the output streams, one request at a time. A stream
/// whose handle was dropped closes before the next stream opens. Each handle holds a sender of the
/// queue, so the thread keeps running while any playback is open.
struct Devices {
    requests: SyncSender<Request>,
}

impl Devices {
    fn spawn() -> Result<Devices, PlatformError> {
        let (requests, rx) = sync_channel(REQUESTS_QUEUED);
        std::thread::Builder::new().name("audio monitor".into()).spawn(move || serve(&rx)).map_err(|e| PlatformError::Failed(e.to_string()))?;
        Ok(Devices { requests })
    }

    /// Queue the open of a stream and return its handle at once.
    fn play(&self, open: OpenStream, fill: Fill, on_error: OnError) -> Result<AudioPlayback, PlatformError> {
        let slot = Arc::new(Slot::default());
        match self.requests.try_send(Request::Open { open, fill, on_error, slot: Pending(Arc::clone(&slot)) }) {
            Ok(()) => Ok(AudioPlayback { slot, requests: self.requests.clone() }),
            Err(TrySendError::Full(_)) => Err(PlatformError::Failed("the audio device is busy".into())),
            Err(TrySendError::Disconnected(_)) => Err(PlatformError::Failed("the audio device thread stopped".into())),
        }
    }
}

/// Serve requests until every sender is gone. A stream stays on this thread from its open to its
/// close. A panic while a stream opens fails only that playback, and a panic while a stream closes
/// or a request is dropped ends only that step, so the thread and the streams that have started
/// keep running.
fn serve(requests: &Receiver<Request>) {
    let mut streams: Vec<(Pending, Box<dyn Any>)> = Vec::new();
    while let Ok(request) = requests.recv() {
        let _ = catch_unwind(AssertUnwindSafe(|| streams.retain(|(slot, _)| !slot.0.dropped.load(Ordering::Acquire))));
        let _ = catch_unwind(AssertUnwindSafe(|| serve_one(&mut streams, request)));
    }
}

/// Open the stream of one request, unless its handle is already gone.
fn serve_one(streams: &mut Vec<(Pending, Box<dyn Any>)>, request: Request) {
    let Request::Open { open, fill, on_error, slot } = request else { return };
    // A handle dropped before its turn needs no device.
    if slot.0.dropped.load(Ordering::Acquire) {
        return;
    }
    match catch_unwind(AssertUnwindSafe(|| open(fill, on_error))) {
        Ok(Ok((stream, format))) => {
            let _ = slot.0.started.set(Ok(format));
            streams.push((slot, stream));
        }
        Ok(Err(e)) => {
            let _ = slot.0.started.set(Err(e.to_string()));
        }
        Err(_) => {
            let _ = slot.0.started.set(Err(OPEN_FAILED.into()));
        }
    }
}

/// Why a playback whose open panicked on the device thread could not start.
const OPEN_FAILED: &str = "the audio device failed while opening";

/// The device thread for the system's devices, started on the first open. The queue's sender
/// stored here is never dropped, so the thread runs until the process ends.
static DEVICES: OnceLock<Devices> = OnceLock::new();

/// The shared device thread, started by the first open. Reading it is one atomic load. Callers
/// that race to start it each start a thread; the first one stored is kept, and the others end when
/// their queues are dropped. No lock is held while a thread starts.
fn shared_devices() -> Result<&'static Devices, PlatformError> {
    if let Some(d) = DEVICES.get() {
        return Ok(d);
    }
    let _ = DEVICES.set(Devices::spawn()?);
    DEVICES.get().ok_or_else(|| PlatformError::Failed("the audio device thread stopped".into()))
}

/// Playback on an output device. The device opens and its stream lives on a device thread, so
/// opening returns at once and the handle can move between threads. Dropping the handle closes
/// the stream on that thread, without waiting for it.
pub struct AudioPlayback {
    slot: Arc<Slot>,
    requests: SyncSender<Request>,
}

impl AudioPlayback {
    /// Play on the output device `id` ([`DEFAULT_DEVICE`] or empty for the default). `fill`
    /// supplies the samples on the device's own thread. Returns while the device opens; an error
    /// here means the open could not be queued.
    pub fn open(id: &str, fill: Fill, on_error: OnError) -> Result<AudioPlayback, PlatformError> {
        let id = id.to_owned();
        let open: OpenStream = Box::new(move |fill, on_error| {
            imp::Playback::open(&id, fill, on_error).map(|(stream, format)| (Box::new(stream) as Box<dyn Any>, format))
        });
        shared_devices()?.play(open, fill, on_error)
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        self.slot.dropped.store(true, Ordering::Release);
        // A full queue has requests waiting, and the thread closes the stream before it serves
        // the next one.
        let _ = self.requests.try_send(Request::Close);
    }
}

impl Playing for AudioPlayback {
    /// The device's format, or zeros while it is still opening or after it failed to open.
    fn format(&self) -> Format {
        self.slot.started.get().and_then(|s| s.as_ref().ok().copied()).unwrap_or(Format { rate: 0, channels: 0 })
    }

    fn started(&self) -> bool {
        self.slot.started.get().is_some_and(|s| s.is_ok())
    }

    fn open_error(&self) -> Option<&str> {
        self.slot.started.get().and_then(|s| s.as_ref().err()).map(String::as_str)
    }
}

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample};

    use super::{DEFAULT_DEVICE, DeviceInfo, DeviceKind, Fill, Format, OnError};
    use crate::PlatformError;

    /// Samples converted per pass for a device whose format is not `f32`.
    const SCRATCH_SAMPLES: usize = 8192;

    fn list(host: &cpal::Host, kind: DeviceKind) -> Vec<cpal::Device> {
        let devices = match kind {
            DeviceKind::Input => host.input_devices().map(|d| d.collect()),
            DeviceKind::Output => host.output_devices().map(|d| d.collect()),
        };
        devices.unwrap_or_default()
    }

    fn default_device(host: &cpal::Host, kind: DeviceKind) -> Option<cpal::Device> {
        match kind {
            DeviceKind::Input => host.default_input_device(),
            DeviceKind::Output => host.default_output_device(),
        }
    }

    pub fn devices(kind: DeviceKind) -> Vec<DeviceInfo> {
        let host = cpal::default_host();
        let default = default_device(&host, kind).and_then(|d| d.name().ok());
        list(&host, kind)
            .iter()
            .filter_map(|d| d.name().ok())
            .map(|name| DeviceInfo { id: name.clone(), default: Some(&name) == default.as_ref(), name })
            .collect()
    }

    pub struct Capture {
        _stream: cpal::Stream,
    }

    impl Capture {
        pub fn open(
            kind: DeviceKind,
            id: &str,
            on_data: impl FnMut(&[f32], Format) + Send + 'static,
            on_error: impl FnMut(String) + Send + 'static,
        ) -> Result<(Capture, Format), PlatformError> {
            if kind == DeviceKind::Output && !cfg!(windows) {
                return Err(PlatformError::Unsupported);
            }
            let host = cpal::default_host();
            let device = if id.is_empty() || id == DEFAULT_DEVICE {
                default_device(&host, kind)
            } else {
                list(&host, kind).into_iter().find(|d| d.name().ok().as_deref() == Some(id))
            };
            let device = device.ok_or_else(|| PlatformError::NotFound(format!("the audio device \u{201c}{id}\u{201d}")))?;
            let supported = match kind {
                DeviceKind::Input => device.default_input_config(),
                DeviceKind::Output => device.default_output_config(),
            }
            .map_err(PlatformError::failed)?;
            let config: cpal::StreamConfig = supported.config();
            let format = Format { rate: config.sample_rate.0, channels: config.channels };
            let stream = match supported.sample_format() {
                SampleFormat::F32 => build::<f32>(&device, &config, format, on_data, on_error),
                SampleFormat::I16 => build::<i16>(&device, &config, format, on_data, on_error),
                SampleFormat::I32 => build::<i32>(&device, &config, format, on_data, on_error),
                SampleFormat::U16 => build::<u16>(&device, &config, format, on_data, on_error),
                SampleFormat::U8 => build::<u8>(&device, &config, format, on_data, on_error),
                other => return Err(PlatformError::Failed(format!("the device's sample format {other:?} is not supported"))),
            }?;
            stream.play().map_err(PlatformError::failed)?;
            Ok((Capture { _stream: stream }, format))
        }
    }

    fn build<T>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        format: Format,
        mut on_data: impl FnMut(&[f32], Format) + Send + 'static,
        mut on_error: impl FnMut(String) + Send + 'static,
    ) -> Result<cpal::Stream, PlatformError>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let mut buf: Vec<f32> = Vec::new();
        device
            .build_input_stream(
                config,
                move |data: &[T], _: &cpal::InputCallbackInfo| {
                    buf.clear();
                    buf.extend(data.iter().map(|s| <f32 as FromSample<T>>::from_sample_(*s)));
                    on_data(&buf, format);
                },
                move |e| on_error(e.to_string()),
                None,
            )
            .map_err(PlatformError::failed)
    }

    /// The device stream of an [`super::AudioPlayback`]; dropping it stops the device.
    pub struct Playback {
        _stream: cpal::Stream,
    }

    impl Playback {
        pub fn open(id: &str, fill: Fill, on_error: OnError) -> Result<(Playback, Format), PlatformError> {
            let host = cpal::default_host();
            let device = if id.is_empty() || id == DEFAULT_DEVICE {
                host.default_output_device()
            } else {
                list(&host, DeviceKind::Output).into_iter().find(|d| d.name().ok().as_deref() == Some(id))
            };
            let device = device.ok_or_else(|| PlatformError::NotFound(format!("the audio device \u{201c}{id}\u{201d}")))?;
            let supported = device.default_output_config().map_err(PlatformError::failed)?;
            let config: cpal::StreamConfig = supported.config();
            let format = Format { rate: config.sample_rate.0, channels: config.channels };
            let stream = match supported.sample_format() {
                SampleFormat::F32 => play::<f32>(&device, &config, format, fill, on_error),
                SampleFormat::I16 => play::<i16>(&device, &config, format, fill, on_error),
                SampleFormat::I32 => play::<i32>(&device, &config, format, fill, on_error),
                SampleFormat::U16 => play::<u16>(&device, &config, format, fill, on_error),
                SampleFormat::U8 => play::<u8>(&device, &config, format, fill, on_error),
                other => return Err(PlatformError::Failed(format!("the device's sample format {other:?} is not supported"))),
            }?;
            stream.play().map_err(PlatformError::failed)?;
            Ok((Playback { _stream: stream }, format))
        }
    }

    fn play<T>(
        device: &cpal::Device,
        config: &cpal::StreamConfig,
        format: Format,
        mut fill: Fill,
        mut on_error: OnError,
    ) -> Result<cpal::Stream, PlatformError>
    where
        T: SizedSample + FromSample<f32>,
    {
        // Allocated here, before the stream starts, so the callback does not allocate.
        let mut scratch = vec![0.0f32; SCRATCH_SAMPLES];
        let channels = usize::from(format.channels.max(1));
        let step = (SCRATCH_SAMPLES / channels) * channels;
        device
            .build_output_stream(
                config,
                move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                    if step == 0 {
                        data.iter_mut().for_each(|d| *d = T::EQUILIBRIUM);
                        return;
                    }
                    for chunk in data.chunks_mut(step) {
                        let Some(buf) = scratch.get_mut(..chunk.len()) else {
                            chunk.iter_mut().for_each(|d| *d = T::EQUILIBRIUM);
                            continue;
                        };
                        fill(buf, format);
                        for (d, s) in chunk.iter_mut().zip(buf.iter()) {
                            *d = <T as FromSample<f32>>::from_sample_(*s);
                        }
                    }
                },
                move |e| on_error(e.to_string()),
                None,
            )
            .map_err(PlatformError::failed)
    }
}

#[cfg(any(target_os = "linux", test))]
mod pw_props;

#[cfg(target_os = "linux")]
#[path = "audio/linux.rs"]
mod imp;

#[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
mod imp {
    use super::{DeviceInfo, DeviceKind, Fill, Format, OnError};
    use crate::PlatformError;

    pub struct Playback;

    impl Playback {
        pub fn open(_id: &str, _fill: Fill, _on_error: OnError) -> Result<(Playback, Format), PlatformError> {
            Err(PlatformError::Unsupported)
        }
    }

    pub fn devices(_kind: DeviceKind) -> Vec<DeviceInfo> {
        Vec::new()
    }

    pub struct Capture;

    impl Capture {
        pub fn open(
            _kind: DeviceKind,
            _id: &str,
            _on_data: impl FnMut(&[f32], Format) + Send + 'static,
            _on_error: impl FnMut(String) + Send + 'static,
        ) -> Result<(Capture, Format), PlatformError> {
            Err(PlatformError::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn listing_devices_does_not_fail() {
        for kind in [DeviceKind::Input, DeviceKind::Output] {
            let list = devices(kind);
            assert!(list.iter().filter(|d| d.default).count() <= 1);
            if let Some(d) = list.iter().find(|d| d.default) {
                assert_eq!(list.first(), Some(d));
            }
        }
    }

    #[test]
    fn an_unknown_device_is_reported() {
        let r = AudioCapture::open(DeviceKind::Input, "no such device 1234", |_, _| {}, |_| {});
        assert!(r.is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_linux_capture_starts_or_reports_why_and_stops_when_dropped() {
        for kind in [DeviceKind::Input, DeviceKind::Output] {
            match AudioCapture::open(kind, DEFAULT_DEVICE, |_, _| {}, |_| {}) {
                // Dropping the handle ends the capture thread; the test would hang otherwise.
                Ok(c) => {
                    assert_eq!(c.format, Format { rate: 48_000, channels: 2 });
                    drop(c);
                }
                // Without a PipeWire server (build machines) the reason is reported.
                Err(e) => assert!(matches!(e, PlatformError::Failed(_)), "{e:?}"),
            }
        }
    }

    #[test]
    fn an_unknown_output_device_is_reported() {
        // The device opens on the device thread, so the reason arrives on the handle.
        if let Ok(p) = SystemSink.play("no such device 1234", Box::new(|_, _| {}), Box::new(|_| {})) {
            while p.open_error().is_none() {
                assert!(!p.started(), "an unknown device does not play");
                std::thread::yield_now();
            }
        }
    }

    /// Opens a held device with the result sent.
    type Release = SyncSender<Result<Format, PlatformError>>;

    const STEREO_48K: Format = Format { rate: 48_000, channels: 2 };

    /// A stand-in for a device stream that says when it is dropped.
    struct Stream(SyncSender<()>);

    impl Drop for Stream {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    /// An open on the device thread that returns `open`'s stream.
    fn opening<S: 'static>(open: impl FnOnce() -> Result<(S, Format), PlatformError> + Send + 'static) -> OpenStream {
        Box::new(move |_, _| open().map(|(stream, format)| (Box::new(stream) as Box<dyn Any>, format)))
    }

    fn play(devices: &Devices, open: OpenStream) -> Result<AudioPlayback, PlatformError> {
        devices.play(open, Box::new(|_, _| {}), Box::new(|_| {}))
    }

    /// A playback whose device opens with the result the test sends on the returned sender, a
    /// receiver that hears when the open has begun, and one that hears when its stream is dropped.
    fn held_open(devices: &Devices) -> (AudioPlayback, Release, Receiver<()>, Receiver<()>) {
        let (release_tx, release_rx) = sync_channel::<Result<Format, PlatformError>>(0);
        let (began_tx, began_rx) = sync_channel(1);
        let (dropped_tx, dropped_rx) = sync_channel(1);
        let open = opening(move || {
            let _ = began_tx.send(());
            let format = release_rx.recv().map_err(|_| PlatformError::Failed("the test ended".into()))??;
            Ok((Stream(dropped_tx), format))
        });
        (play(devices, open).unwrap(), release_tx, began_rx, dropped_rx)
    }

    #[test]
    fn playback_returns_while_the_device_is_still_opening() {
        let devices = Devices::spawn().unwrap();
        let (p, release, _began, dropped) = held_open(&devices);
        assert_eq!((p.started(), p.open_error()), (false, None), "the device is still opening");
        release.send(Ok(STEREO_48K)).unwrap();
        while !p.started() {
            std::thread::yield_now();
        }
        assert_eq!(p.format(), STEREO_48K);
        drop(p);
        dropped.recv().unwrap();
    }

    #[test]
    fn a_device_that_fails_to_open_reports_why_on_the_handle() {
        let devices = Devices::spawn().unwrap();
        let (p, release, _began, _dropped) = held_open(&devices);
        release.send(Err(PlatformError::Failed("the device is busy".into()))).unwrap();
        while p.open_error().is_none() {
            std::thread::yield_now();
        }
        assert_eq!((p.started(), p.open_error()), (false, Some("the device is busy")));
    }

    #[test]
    fn dropping_a_playback_does_not_wait_for_the_device_to_open() {
        let devices = Devices::spawn().unwrap();
        let (p, release, began, dropped) = held_open(&devices);
        began.recv().unwrap();
        drop(p);
        // The open finishes after the handle is gone, and its stream is stopped straight away.
        release.send(Ok(STEREO_48K)).unwrap();
        dropped.recv().unwrap();
    }

    #[test]
    fn a_device_that_takes_long_to_open_makes_later_opens_busy_without_blocking() {
        let devices = Devices::spawn().unwrap();
        let (first, release, began, _dropped) = held_open(&devices);
        began.recv().unwrap();
        let queued: Vec<AudioPlayback> = (0..REQUESTS_QUEUED).map(|_| play(&devices, opening(|| Ok(((), STEREO_48K)))).unwrap()).collect();
        let busy = play(&devices, opening(|| Ok(((), STEREO_48K))));
        assert_eq!(busy.err(), Some(PlatformError::Failed("the audio device is busy".into())));
        release.send(Ok(STEREO_48K)).unwrap();
        for p in &queued {
            while !p.started() {
                std::thread::yield_now();
            }
        }
        assert!(first.started());
    }

    #[test]
    fn an_open_that_panics_fails_only_its_own_playback() {
        let devices = Devices::spawn().unwrap();
        let (kept, release, _began, kept_dropped) = held_open(&devices);
        release.send(Ok(STEREO_48K)).unwrap();
        while !kept.started() {
            std::thread::yield_now();
        }
        let (release_tx, release_rx) = sync_channel::<()>(0);
        let (began_tx, began_rx) = sync_channel(1);
        let crashing = play(
            &devices,
            opening(move || -> Result<((), Format), PlatformError> {
                let _ = began_tx.send(());
                let _ = release_rx.recv();
                panic!("the device driver crashed")
            }),
        )
        .unwrap();
        began_rx.recv().unwrap();
        let queued = play(&devices, opening(|| Ok(((), STEREO_48K)))).unwrap();
        release_tx.send(()).unwrap();
        while crashing.open_error().is_none() {
            std::thread::yield_now();
        }
        assert_eq!((crashing.started(), crashing.open_error()), (false, Some(OPEN_FAILED)));
        while !queued.started() && queued.open_error().is_none() {
            std::thread::yield_now();
        }
        assert_eq!((queued.started(), queued.open_error()), (true, None), "the open queued behind it still runs");
        assert!(kept.started());
        assert!(kept_dropped.try_recv().is_err(), "the stream that had started was closed");
        let after = play(&devices, opening(|| Ok(((), STEREO_48K)))).unwrap();
        while !after.started() {
            std::thread::yield_now();
        }
        drop(kept);
        kept_dropped.recv().unwrap();
    }

    /// Panics when it is dropped.
    struct PanicOnDrop;

    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("the device driver crashed while closing");
        }
    }

    #[test]
    fn a_panic_while_a_skipped_request_is_dropped_leaves_the_other_streams_open() {
        let devices = Devices::spawn().unwrap();
        let (kept, release, began, kept_dropped) = held_open(&devices);
        began.recv().unwrap();
        let crashes_on_drop = PanicOnDrop;
        let fill: Fill = Box::new(move |_, _| {
            let _ = &crashes_on_drop;
        });
        let skipped = devices.play(opening(|| Ok(((), STEREO_48K))), fill, Box::new(|_| {})).unwrap();
        drop(skipped);
        let after = play(&devices, opening(|| Ok(((), STEREO_48K)))).unwrap();
        release.send(Ok(STEREO_48K)).unwrap();
        while !after.started() && after.open_error().is_none() {
            std::thread::yield_now();
        }
        assert_eq!((after.started(), after.open_error()), (true, None));
        assert!(kept_dropped.try_recv().is_err(), "the stream that had started was closed");
        drop(kept);
        kept_dropped.recv().unwrap();
    }

    #[test]
    fn a_panic_while_a_stream_closes_leaves_the_other_streams_open() {
        let devices = Devices::spawn().unwrap();
        let (kept, release, _began, kept_dropped) = held_open(&devices);
        release.send(Ok(STEREO_48K)).unwrap();
        let crashing = play(&devices, opening(|| Ok((PanicOnDrop, STEREO_48K)))).unwrap();
        while !crashing.started() {
            std::thread::yield_now();
        }
        drop(crashing);
        let after = play(&devices, opening(|| Ok(((), STEREO_48K)))).unwrap();
        while !after.started() {
            std::thread::yield_now();
        }
        assert!(kept_dropped.try_recv().is_err(), "the stream that had started was closed");
        drop(kept);
        kept_dropped.recv().unwrap();
    }

    #[test]
    fn every_system_playback_shares_one_device_thread() {
        let first = std::thread::spawn(shared_devices);
        let second = shared_devices().unwrap();
        assert!(std::ptr::eq(first.join().unwrap().unwrap(), second));
    }

    #[test]
    fn a_playback_dropped_before_its_turn_does_not_open_the_device() {
        let devices = Devices::spawn().unwrap();
        let (first, release, began, _dropped) = held_open(&devices);
        began.recv().unwrap();
        let opened = Arc::new(AtomicBool::new(false));
        let mark = Arc::clone(&opened);
        let skipped = play(
            &devices,
            opening(move || {
                mark.store(true, Ordering::SeqCst);
                Ok(((), STEREO_48K))
            }),
        )
        .unwrap();
        drop(skipped);
        let last = play(&devices, opening(|| Ok(((), STEREO_48K)))).unwrap();
        release.send(Ok(STEREO_48K)).unwrap();
        while !last.started() {
            std::thread::yield_now();
        }
        assert!(first.started());
        assert!(!opened.load(Ordering::SeqCst), "a dropped playback's device was opened");
    }

    /// A stand-in for a device stream whose close waits for the test, and then marks it closed.
    struct SlowClose {
        began: SyncSender<()>,
        finish: Receiver<()>,
        closed: Arc<AtomicBool>,
    }

    impl Drop for SlowClose {
        fn drop(&mut self) {
            let _ = self.began.send(());
            let _ = self.finish.recv();
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn the_next_playback_opens_after_the_dropped_stream_has_closed() {
        let devices = Devices::spawn().unwrap();
        let closed = Arc::new(AtomicBool::new(false));
        let (began_tx, began_rx) = sync_channel(1);
        let (finish_tx, finish_rx) = sync_channel(1);
        let stream = SlowClose { began: began_tx, finish: finish_rx, closed: Arc::clone(&closed) };
        let first = play(&devices, opening(move || Ok((stream, STEREO_48K)))).unwrap();
        while !first.started() {
            std::thread::yield_now();
        }
        drop(first);
        let (seen_tx, seen_rx) = sync_channel(1);
        let was_closed = Arc::clone(&closed);
        let open = opening(move || {
            let _ = seen_tx.send(was_closed.load(Ordering::SeqCst));
            Ok(((), STEREO_48K))
        });
        // The second playback returns while the first stream is still closing.
        let second = play(&devices, open).unwrap();
        began_rx.recv().unwrap();
        assert!(!second.started());
        finish_tx.send(()).unwrap();
        assert!(seen_rx.recv().unwrap(), "the device was opened again before the old stream closed");
        while !second.started() {
            std::thread::yield_now();
        }
    }
}
