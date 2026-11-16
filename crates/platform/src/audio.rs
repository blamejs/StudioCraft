//! Audio devices: listing, input capture, and capture of what an output device plays (Windows).

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

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample};

    use super::{DEFAULT_DEVICE, DeviceInfo, DeviceKind, Format};
    use crate::PlatformError;

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
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use super::{DeviceInfo, DeviceKind, Format};
    use crate::PlatformError;

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
}
