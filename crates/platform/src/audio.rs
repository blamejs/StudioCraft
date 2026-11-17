//! Audio devices: listing, input capture, capture of what an output device plays (Windows and
//! Linux), and playback on an output device (Windows and macOS).
//!
//! Windows and macOS use cpal. Linux uses the PipeWire sound server: capture asks for 48 kHz
//! stereo, and PipeWire converts each device's own format to it.

use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread::JoinHandle;

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
    /// Playback runs until the returned handle is dropped.
    fn play(&self, id: &str, fill: Fill, on_error: OnError) -> Result<Box<dyn Playing>, PlatformError>;
}

/// An output device that is playing; dropping it stops the device.
pub trait Playing: Send {
    /// The device's sample rate and channel count.
    fn format(&self) -> Format;
}

/// The system's output devices.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemSink;

impl AudioSink for SystemSink {
    fn play(&self, id: &str, fill: Fill, on_error: OnError) -> Result<Box<dyn Playing>, PlatformError> {
        Ok(Box::new(AudioPlayback::open(id, fill, on_error)?))
    }
}

/// Playback on an output device. The device stream lives on a thread of its own, so the handle
/// can move between threads; dropping the handle stops the stream and ends the thread.
pub struct AudioPlayback {
    stop: Option<SyncSender<()>>,
    thread: Option<JoinHandle<()>>,
    pub format: Format,
}

impl AudioPlayback {
    /// Play on the output device `id` ([`DEFAULT_DEVICE`] or empty for the default). `fill`
    /// supplies the samples on the device's own thread. Returns once the device has started, or
    /// with the reason it could not.
    pub fn open(id: &str, fill: Fill, on_error: OnError) -> Result<AudioPlayback, PlatformError> {
        let (ready_tx, ready_rx) = sync_channel(1);
        let (stop_tx, stop_rx) = sync_channel::<()>(0);
        let id = id.to_owned();
        let thread = std::thread::Builder::new()
            .name("audio monitor".into())
            .spawn(move || match imp::Playback::open(&id, fill, on_error) {
                // The stream plays until the handle is dropped, and stops on this thread at the end
                // of the arm.
                Ok((_stream, format)) => {
                    let _ = ready_tx.send(Ok(format));
                    let _ = stop_rx.recv();
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| PlatformError::Failed(e.to_string()))?;
        match ready_rx.recv() {
            Ok(Ok(format)) => Ok(AudioPlayback { stop: Some(stop_tx), thread: Some(thread), format }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(PlatformError::Failed("the audio device thread stopped".into()))
            }
        }
    }
}

impl Playing for AudioPlayback {
    fn format(&self) -> Format {
        self.format
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
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
        let r = SystemSink.play("no such device 1234", Box::new(|_, _| {}), Box::new(|_| {}));
        assert!(r.is_err());
    }
}
