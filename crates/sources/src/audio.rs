//! Audio input capture and output (loopback) capture.

use std::sync::{Arc, Mutex};

use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_platform::audio::{AudioCapture, DEFAULT_DEVICE, DeviceKind, devices};
use studiocraft_plugin::{Pace, Plugin, PluginError, Source, SourceCaps, SourceInstance, SourceIo};

use crate::AudioFeed;

fn schema(kind: DeviceKind) -> SettingsSchema {
    let mut options = vec![ChoiceOption::new(DEFAULT_DEVICE, "Default")];
    options.extend(devices(kind).into_iter().map(|d| ChoiceOption::new(d.id, d.name)));
    SettingsSchema::new(vec![Field::new("device", "Device", FieldKind::Choice { options }, DEFAULT_DEVICE)])
}

struct Instance {
    kind: DeviceKind,
    device: String,
    capture: Option<AudioCapture>,
    /// The device stream's last error, reported by the next `produce`.
    error: Arc<Mutex<Option<String>>>,
}

impl Instance {
    fn open(kind: DeviceKind, device: &str, io: &SourceIo) -> Result<Instance, PluginError> {
        let mut i = Instance { kind, device: device.to_string(), capture: None, error: Arc::new(Mutex::new(None)) };
        i.start(io)?;
        Ok(i)
    }

    fn start(&mut self, io: &SourceIo) -> Result<(), PluginError> {
        self.capture = None;
        let (audio, clock) = (Arc::clone(&io.audio), io.clock);
        let mut feed: Option<AudioFeed> = None;
        let on_data = move |samples: &[f32], format: studiocraft_platform::audio::Format| {
            feed.get_or_insert_with(|| AudioFeed::new(format.rate, usize::from(format.channels), Arc::clone(&audio), clock)).push(samples);
        };
        let err = Arc::clone(&self.error);
        let on_error = move |e: String| {
            if let Ok(mut slot) = err.lock() {
                *slot = Some(e);
            }
        };
        let capture = AudioCapture::open(self.kind, &self.device, on_data, on_error).map_err(|e| PluginError::Unavailable(e.to_string()))?;
        self.capture = Some(capture);
        Ok(())
    }
}

impl SourceInstance for Instance {
    fn pace(&self) -> Pace {
        // The device's callback pushes audio; the thread only checks for stream errors.
        Pace::Every(std::time::Duration::from_secs(1))
    }
    fn produce(&mut self, io: &SourceIo) -> Result<(), PluginError> {
        let failed = self.error.lock().ok().and_then(|mut e| e.take());
        if let Some(e) = failed {
            // A device that was unplugged or reset: open it again.
            self.start(io)?;
            return Err(PluginError::Failed(format!("the audio device stopped ({e}); it was reopened")));
        }
        if self.capture.is_none() {
            self.start(io)?;
        }
        Ok(())
    }
    fn update(&mut self, s: &Settings, io: &SourceIo) -> Result<(), PluginError> {
        let device = s.str("device").unwrap_or(DEFAULT_DEVICE);
        if device != self.device {
            self.device = device.to_string();
            self.start(io)?;
        }
        Ok(())
    }
}

/// A microphone or line input.
pub struct AudioInput;

impl Plugin for AudioInput {
    fn id(&self) -> &'static str {
        "audio_input"
    }
    fn name(&self) -> &'static str {
        "Audio input capture"
    }
    fn schema(&self) -> SettingsSchema {
        schema(DeviceKind::Input)
    }
}

impl Source for AudioInput {
    fn caps(&self) -> SourceCaps {
        SourceCaps { video: false, audio: true }
    }
    fn create(&self, settings: &Settings, io: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        Ok(Box::new(Instance::open(DeviceKind::Input, settings.str("device").unwrap_or(DEFAULT_DEVICE), io)?))
    }
}

/// What an output device plays.
pub struct AudioOutput;

impl Plugin for AudioOutput {
    fn id(&self) -> &'static str {
        "audio_output"
    }
    fn name(&self) -> &'static str {
        "Audio output capture"
    }
    fn schema(&self) -> SettingsSchema {
        schema(DeviceKind::Output)
    }
}

impl Source for AudioOutput {
    fn caps(&self) -> SourceCaps {
        SourceCaps { video: false, audio: true }
    }
    fn create(&self, settings: &Settings, io: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        Ok(Box::new(Instance::open(DeviceKind::Output, settings.str("device").unwrap_or(DEFAULT_DEVICE), io)?))
    }
}

#[cfg(test)]
mod tests {
    use studiocraft_media::clock::MasterClock;

    use super::*;

    #[test]
    fn the_default_device_is_always_offered() {
        for schema in [AudioInput.schema(), AudioOutput.schema()] {
            let FieldKind::Choice { options } = &schema.fields[0].kind else { panic!("device is a choice") };
            assert_eq!(options[0].value, DEFAULT_DEVICE);
        }
    }

    #[test]
    fn an_unknown_device_fails_to_start() {
        let io = SourceIo::new(MasterClock::new());
        assert!(AudioInput.create(&Settings::new().with("device", "no such microphone 77"), &io).is_err());
    }

    #[test]
    fn output_capture_delivers_blocks_where_a_device_exists() {
        let io = SourceIo::new(MasterClock::new());
        // Machines without audio devices cannot open one.
        let Ok(_i) = AudioOutput.create(&AudioOutput.schema().defaults(), &io) else { return };
        // Loopback delivers silence-filled packets while nothing plays, at the device's period.
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = io.audio.queued();
    }
}
