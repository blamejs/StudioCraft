//! A picture file.

use std::io::Read;
use std::path::Path;

use studiocraft_media::video::{MAX_FRAME_SIDE, PixelFormat};
use studiocraft_model::{Field, FieldKind, PathMode, Settings, SettingsSchema};
use studiocraft_plugin::{Pace, Plugin, PluginError, Source, SourceCaps, SourceInstance, SourceIo, VideoContent};

/// Largest picture file read, in bytes.
const MAX_FILE: u64 = 512 * 1024 * 1024;
/// Most pixels in a picture: 8192 x 8192, which is 256 MB as RGBA.
const MAX_PIXELS: u64 = 8192 * 8192;

/// The picture file extensions the file dialogs offer.
pub(crate) const PICTURE_EXTENSIONS: [&str; 9] = ["png", "jpg", "jpeg", "bmp", "gif", "webp", "tga", "tif", "tiff"];

pub struct ImageSource;

impl Plugin for ImageSource {
    fn id(&self) -> &'static str {
        "image"
    }
    fn name(&self) -> &'static str {
        "Image"
    }
    fn schema(&self) -> SettingsSchema {
        let extensions = PICTURE_EXTENSIONS.map(String::from).to_vec();
        SettingsSchema::new(vec![Field::new(
            "file",
            "Picture file",
            FieldKind::Path { mode: PathMode::OpenFile, filter_name: "Pictures".into(), extensions },
            "",
        )])
    }
}

impl Source for ImageSource {
    fn caps(&self) -> SourceCaps {
        SourceCaps { video: true, audio: false }
    }
    fn create(&self, settings: &Settings, _io: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        Ok(Box::new(Instance { file: settings.str("file").unwrap_or_default().to_string() }))
    }
}

struct Instance {
    file: String,
}

/// Decode `path` to straight-alpha RGBA.
pub(crate) fn load(path: &Path) -> Result<(u32, u32, Vec<u8>), PluginError> {
    load_within(path, MAX_FILE, MAX_PIXELS)
}

/// Open `path` for reading, refusing anything that is not a file, such as a folder, a device or
/// a named pipe. On Unix the file is opened without waiting, so a named pipe with no writer is
/// refused at once instead of holding the caller.
pub(crate) fn open_file(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "it is not a file"));
    }
    Ok(file)
}

/// Decode `path` to straight-alpha RGBA, refusing anything that is not a file, a file larger
/// than `max_file` bytes or a picture with more than `max_pixels` pixels before reading or
/// decoding it.
pub(crate) fn load_within(path: &Path, max_file: u64, max_pixels: u64) -> Result<(u32, u32, Vec<u8>), PluginError> {
    let failed = |e: std::io::Error| PluginError::Failed(format!("{}: {e}", path.display()));
    let too_large = || PluginError::Failed(format!("{} is too large", path.display()));
    let file = open_file(path).map_err(failed)?;
    if file.metadata().map_err(failed)?.len() > max_file {
        return Err(too_large());
    }
    // The file can grow after the size check; the read stops one byte past the cap.
    let mut bytes = Vec::new();
    file.take(max_file + 1).read_to_end(&mut bytes).map_err(failed)?;
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > max_file {
        return Err(too_large());
    }
    // The decoder refuses a picture wider or taller than a frame, or with more than
    // `max_pixels` pixels, before it allocates them.
    let mut opts = craft_image_codecs::DecodeOptions::default();
    opts.limits.max_width = MAX_FRAME_SIDE;
    opts.limits.max_height = MAX_FRAME_SIDE;
    opts.limits.max_pixels = max_pixels;
    opts.limits.max_alloc = max_pixels.saturating_mul(8);
    let img = craft_image_codecs::decode_with(&bytes, &opts).map_err(|e| PluginError::Failed(format!("{}: {e}", path.display())))?;
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 || w > MAX_FRAME_SIDE || h > MAX_FRAME_SIDE {
        return Err(PluginError::Failed(format!("{} is {w} x {h} pixels, which is not supported", path.display())));
    }
    Ok((w, h, img.to_rgba8()))
}

impl SourceInstance for Instance {
    fn pace(&self) -> Pace {
        Pace::Static
    }
    fn produce(&mut self, io: &SourceIo) -> Result<(), PluginError> {
        if self.file.trim().is_empty() {
            io.video.publish(VideoContent::Empty);
            return Ok(());
        }
        let (w, h, rgba) = match load(Path::new(&self.file)) {
            Ok(picture) => picture,
            Err(e) => {
                // The picture of an earlier file is not shown in place of this one.
                io.video.publish(VideoContent::Empty);
                return Err(e);
            }
        };
        let mut frame = io.video.frame();
        if !frame.reshape(w, h, PixelFormat::Rgba8) || frame.data.len() != rgba.len() {
            return Err(PluginError::Failed(format!("{} could not be decoded", self.file)));
        }
        frame.data.copy_from_slice(&rgba);
        frame.pts = io.clock.now();
        io.video.publish(VideoContent::Frame(frame));
        Ok(())
    }
    fn update(&mut self, settings: &Settings, _io: &SourceIo) -> Result<(), PluginError> {
        self.file = settings.str("file").unwrap_or_default().to_string();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use studiocraft_media::clock::MasterClock;

    use super::*;

    #[test]
    fn decodes_a_picture_file() {
        // A 2 x 1 picture: one red pixel, one half-transparent blue pixel.
        let dir = std::env::temp_dir().join(format!("studiocraft-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("two.png");
        ::image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 128]).unwrap().save(&path).unwrap();
        let io = SourceIo::new(MasterClock::new());
        let mut i = ImageSource.create(&Settings::new().with("file", path.to_string_lossy().as_ref()), &io).unwrap();
        let r = i.produce(&io);
        let _ = std::fs::remove_dir_all(&dir);
        r.unwrap();
        let Some(VideoContent::Frame(f)) = io.video.take() else { panic!("no frame") };
        assert_eq!((f.width, f.height), (2, 1));
        assert_eq!(f.data, [255, 0, 0, 255, 0, 0, 255, 128]);
    }

    /// CMYK JPEGs whose cyan component is sampled below the black one, in each layout the
    /// encoder writes, decode to an opaque picture of the stored size or give an error; none of
    /// them stops the source with a panic.
    #[test]
    fn decodes_four_component_jpegs_whose_first_component_is_subsampled() {
        use jpeg_encoder::{ColorType, Encoder, SamplingFactor as S};
        let dir = std::env::temp_dir().join(format!("studiocraft-image-cmyk-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut results = Vec::new();
        for (s, progressive) in
            [(S::F_2_2, true), (S::R_4_2_0, true), (S::F_4_1, false), (S::F_4_2, false), (S::F_2_4, false), (S::R_4_1_1, false), (S::R_4_1_0, false)]
        {
            for (w, h) in [(1u16, 1u16), (9, 1), (1, 9), (17, 13), (40, 40)] {
                let inks: Vec<u8> = (0..usize::from(w) * usize::from(h)).flat_map(|_| [30u8, 90, 150, 210]).collect();
                let mut out = Vec::new();
                let mut e = Encoder::new(&mut out, 100);
                e.set_sampling_factor(s);
                e.set_progressive(progressive);
                e.encode(&inks, w, h, ColorType::Cmyk).unwrap();
                let path = dir.join(format!("cmyk-{s:?}-{progressive}-{w}x{h}.jpg"));
                std::fs::write(&path, &out).unwrap();
                results.push((format!("{s:?} progressive {progressive} {w}x{h}"), (u32::from(w), u32::from(h)), load(&path)));
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        let decoded = results.iter().filter(|r| r.2.is_ok()).count();
        assert!(decoded >= 20, "{decoded} of {} decoded", results.len());
        for (name, size, r) in results {
            let Ok((w, h, rgba)) = r else { continue };
            assert_eq!((w, h), size, "{name}");
            assert_eq!(rgba.len(), size.0 as usize * size.1 as usize * 4, "{name}");
            assert!(rgba.as_chunks::<4>().0.iter().all(|p| p[3] == 255), "{name}");
        }
    }

    #[test]
    fn a_file_that_cannot_be_read_replaces_the_earlier_picture_with_nothing() {
        let dir = std::env::temp_dir().join(format!("studiocraft-image-stale-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("one.png");
        ::image::RgbaImage::from_raw(1, 1, vec![1, 2, 3, 255]).unwrap().save(&path).unwrap();
        let io = SourceIo::new(MasterClock::new());
        let mut i = ImageSource.create(&Settings::new().with("file", path.to_string_lossy().as_ref()), &io).unwrap();
        let shown = i.produce(&io);
        i.update(&Settings::new().with("file", dir.join("missing.png").to_string_lossy().as_ref()), &io).unwrap();
        let missing = i.produce(&io);
        let _ = std::fs::remove_dir_all(&dir);
        shown.unwrap();
        assert!(missing.is_err());
        assert!(matches!(io.video.take(), Some(VideoContent::Empty)));
    }

    #[test]
    fn a_picture_with_more_pixels_than_the_cap_is_refused() {
        let dir = std::env::temp_dir().join(format!("studiocraft-image-many-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("many.png");
        // 9000 x 8000 is within the side limit and over the pixel cap.
        ::image::GrayImage::new(9000, 8000).save(&path).unwrap();
        let r = load(&path);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(r.is_err());
    }

    #[test]
    fn a_missing_file_is_an_error_and_no_file_shows_nothing() {
        let io = SourceIo::new(MasterClock::new());
        let mut i = ImageSource.create(&Settings::new().with("file", "Z:/missing.png"), &io).unwrap();
        assert!(i.produce(&io).is_err());
        i.update(&Settings::new(), &io).unwrap();
        i.produce(&io).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Empty)));
    }

    #[test]
    fn a_picture_wider_than_a_frame_is_refused_by_the_decoder() {
        let dir = std::env::temp_dir().join(format!("studiocraft-image-wide-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wide.png");
        ::image::GrayImage::new(MAX_FRAME_SIDE + 1, 1).save(&path).unwrap();
        let r = load(&path);
        let _ = std::fs::remove_dir_all(&dir);
        let e = r.err().map(|e| e.to_string()).unwrap_or_default();
        assert!(!e.is_empty(), "the picture was decoded");
        // The size check after decoding names the size; the decoder's limit stops it first.
        assert!(!e.contains("which is not supported"), "{e}");
    }
}
