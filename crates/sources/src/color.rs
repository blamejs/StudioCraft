//! A solid color.

use studiocraft_model::{Field, FieldKind, Settings, SettingsSchema};
use studiocraft_plugin::{Pace, Plugin, PluginError, Source, SourceCaps, SourceInstance, SourceIo, VideoContent};

use crate::uint;

pub struct ColorSource;

impl Plugin for ColorSource {
    fn id(&self) -> &'static str {
        "color"
    }
    fn name(&self) -> &'static str {
        "Color"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::new(vec![
            Field::new("color", "Color", FieldKind::Color, "#d0d0d0ff"),
            Field::new("width", "Width", FieldKind::Int { min: 1, max: 16384 }, 1920),
            Field::new("height", "Height", FieldKind::Int { min: 1, max: 16384 }, 1080),
        ])
    }
}

impl Source for ColorSource {
    fn caps(&self) -> SourceCaps {
        SourceCaps { video: true, audio: false }
    }
    fn create(&self, settings: &Settings, _io: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        let mut i = Instance { content: ([0; 4], 1, 1) };
        i.read(settings);
        Ok(Box::new(i))
    }
}

struct Instance {
    content: ([u8; 4], u32, u32),
}

impl Instance {
    fn read(&mut self, s: &Settings) {
        let rgba = s.color("color").unwrap_or([208, 208, 208, 255]);
        self.content = (rgba, uint(s, "width", 1920, 1, 16384), uint(s, "height", 1080, 1, 16384));
    }
}

impl SourceInstance for Instance {
    fn pace(&self) -> Pace {
        Pace::Static
    }
    fn produce(&mut self, io: &SourceIo) -> Result<(), PluginError> {
        let (rgba, width, height) = self.content;
        io.video.publish(VideoContent::Solid { rgba, width, height });
        Ok(())
    }
    fn update(&mut self, settings: &Settings, _io: &SourceIo) -> Result<(), PluginError> {
        self.read(settings);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use studiocraft_media::clock::MasterClock;

    use super::*;

    #[test]
    fn publishes_its_color_and_size() {
        let io = SourceIo::new(MasterClock::new());
        let s = Settings::new().with("color", "#ff000080").with("width", 100).with("height", 50);
        let mut i = ColorSource.create(&s, &io).unwrap();
        i.produce(&io).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Solid { rgba: [255, 0, 0, 128], width: 100, height: 50 })));
        i.update(&ColorSource.schema().defaults(), &io).unwrap();
        i.produce(&io).unwrap();
        assert_eq!(io.video.size(), Some((1920, 1080)));
    }
}
