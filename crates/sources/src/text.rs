//! Text in an installed font.
//!
//! Fonts come from the system's installed fonts through craft-font-catalog; glyphs are drawn with
//! ab_glyph. Text is laid out left to right, one line per line of the setting, without complex
//! shaping (see ROADMAP.md).

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use craft_font_catalog::Catalog;
use studiocraft_media::video::PixelFormat;
use studiocraft_model::{ChoiceOption, Field, FieldKind, Settings, SettingsSchema};
use studiocraft_plugin::{Pace, Plugin, PluginError, Source, SourceCaps, SourceInstance, SourceIo, VideoContent};

use crate::uint;

/// Largest side of the picture a text source makes.
const MAX_SIDE: u32 = 8192;
/// Most pixels in the picture a text source makes (3840 x 2160 twice over). The coverage mask, the
/// outline mask and the picture each take 4 bytes a pixel.
const MAX_PIXELS: u64 = 3840 * 2160 * 2;
/// Families tried, in order, when the chosen font is not installed.
const FALLBACK_FAMILIES: [&str; 6] = ["Segoe UI", "Arial", "Helvetica", "DejaVu Sans", "Liberation Sans", "Noto Sans"];

pub struct TextSource;

impl Plugin for TextSource {
    fn id(&self) -> &'static str {
        "text"
    }
    fn name(&self) -> &'static str {
        "Text"
    }
    fn schema(&self) -> SettingsSchema {
        let align = vec![ChoiceOption::new("left", "Left"), ChoiceOption::new("center", "Center"), ChoiceOption::new("right", "Right")];
        SettingsSchema::new(vec![
            Field::new("text", "Text", FieldKind::Text { multiline: true }, "Text"),
            Field::new("font", "Font", FieldKind::Font, ""),
            Field::new("size", "Size", FieldKind::Int { min: 6, max: 1024 }, 96),
            Field::new("color", "Color", FieldKind::Color, "#ffffffff"),
            Field::new("align", "Alignment", FieldKind::Choice { options: align }, "left"),
            Field::new("outline", "Outline width", FieldKind::Int { min: 0, max: 64 }, 0),
            Field::new("outline_color", "Outline color", FieldKind::Color, "#000000ff"),
            Field::new("background", "Background", FieldKind::Color, "#00000000"),
        ])
    }
}

impl Source for TextSource {
    fn caps(&self) -> SourceCaps {
        SourceCaps { video: true, audio: false }
    }
    fn create(&self, settings: &Settings, _io: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        Ok(Box::new(Instance { settings: settings.clone() }))
    }
}

struct Instance {
    settings: Settings,
}

impl SourceInstance for Instance {
    fn pace(&self) -> Pace {
        Pace::Static
    }
    fn produce(&mut self, io: &SourceIo) -> Result<(), PluginError> {
        let style = Style::read(&self.settings);
        let text = self.settings.str("text").unwrap_or_default().replace("\r\n", "\n");
        if text.trim().is_empty() {
            io.video.publish(VideoContent::Empty);
            return Ok(());
        }
        let drawn = (|| {
            let font = load_font(self.settings.str("font").unwrap_or_default())?;
            let face = FontRef::try_from_slice_and_index(&font.bytes, font.index)
                .map_err(|e| PluginError::Failed(format!("the font could not be read: {e}")))?;
            let mut frame = io.video.frame();
            let (w, h) = render(&face, &text, &style, &mut frame.data)?;
            frame.width = w;
            frame.height = h;
            frame.format = PixelFormat::Rgba8;
            frame.pts = io.clock.now();
            Ok(frame)
        })();
        match drawn {
            Ok(frame) => {
                io.video.publish(VideoContent::Frame(frame));
                Ok(())
            }
            Err(e) => {
                // The text drawn before the change is not shown as the current text.
                io.video.publish(VideoContent::Empty);
                Err(e)
            }
        }
    }
    fn update(&mut self, settings: &Settings, _io: &SourceIo) -> Result<(), PluginError> {
        self.settings = settings.clone();
        Ok(())
    }
}

pub(crate) fn load_font(family: &str) -> Result<craft_font_catalog::FontData, PluginError> {
    let catalog = Catalog::global();
    if !catalog.is_ready() {
        catalog.rescan();
    }
    let wanted = std::iter::once(family).filter(|f| !f.trim().is_empty()).chain(FALLBACK_FAMILIES);
    let face = wanted
        .filter_map(|f| catalog.resolve(f, "Regular").map(|(face, _)| face))
        .next()
        .or_else(|| catalog.families().first().and_then(|f| catalog.resolve(f, "Regular")).map(|(face, _)| face))
        .ok_or_else(|| PluginError::Unavailable("no fonts are installed".into()))?;
    catalog.load(&face).map_err(|e| PluginError::Failed(format!("the font could not be loaded: {e}")))
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Style {
    size: f32,
    color: [u8; 4],
    align: f32,
    outline: u32,
    outline_color: [u8; 4],
    background: [u8; 4],
}

impl Style {
    pub(crate) fn read(s: &Settings) -> Style {
        Style {
            size: uint(s, "size", 96, 6, 1024) as f32,
            color: s.color("color").unwrap_or([255; 4]),
            align: match s.str("align") {
                Some("center") => 0.5,
                Some("right") => 1.0,
                _ => 0.0,
            },
            outline: uint(s, "outline", 0, 0, 64),
            outline_color: s.color("outline_color").unwrap_or([0, 0, 0, 255]),
            background: s.color("background").unwrap_or([0; 4]),
        }
    }
}

/// Draw `text` into `out` as straight-alpha RGBA; the picture's size.
pub(crate) fn render<F: Font>(font: &F, text: &str, style: &Style, out: &mut Vec<u8>) -> Result<(u32, u32), PluginError> {
    let scaled = font.as_scaled(PxScale::from(style.size));
    let line_height = scaled.ascent() - scaled.descent() + scaled.line_gap();
    let widths: Vec<f32> = text
        .lines()
        .map(|line| {
            let mut w = 0.0;
            let mut prev = None;
            for c in line.chars() {
                let id = scaled.glyph_id(c);
                if let Some(p) = prev {
                    w += scaled.kern(p, id);
                }
                w += scaled.h_advance(id);
                prev = Some(id);
            }
            w
        })
        .collect();
    let pad = style.outline as f32 + 2.0;
    let max_w = widths.iter().copied().fold(0.0f32, f32::max);
    let width = ((max_w + 2.0 * pad).ceil() as u32).clamp(1, MAX_SIDE);
    let height = ((widths.len().max(1) as f32 * line_height + 2.0 * pad).ceil() as u32).clamp(1, MAX_SIDE);
    if u64::from(width) * u64::from(height) > MAX_PIXELS {
        return Err(PluginError::Failed(format!(
            "the text is {width} x {height} pixels, more than {MAX_PIXELS} pixels; use a smaller size or fewer lines"
        )));
    }
    let (wu, hu) = (width as usize, height as usize);

    let mut glyphs = vec![0.0f32; wu * hu];
    for (i, (line, lw)) in text.lines().zip(&widths).enumerate() {
        let mut x = pad + (max_w - lw) * style.align;
        let baseline = pad + scaled.ascent() + i as f32 * line_height;
        let mut prev = None;
        for c in line.chars() {
            let id = scaled.glyph_id(c);
            if let Some(p) = prev {
                x += scaled.kern(p, id);
            }
            let glyph = id.with_scale_and_position(style.size, point(x, baseline));
            x += scaled.h_advance(id);
            prev = Some(id);
            let Some(outlined) = font.outline_glyph(glyph) else { continue };
            let b = outlined.px_bounds();
            outlined.draw(|gx, gy, cov| {
                let (px, py) = (b.min.x as i64 + i64::from(gx), b.min.y as i64 + i64::from(gy));
                if px >= 0 && py >= 0 && (px as usize) < wu && (py as usize) < hu {
                    let slot = &mut glyphs[py as usize * wu + px as usize];
                    *slot = slot.max(cov.clamp(0.0, 1.0));
                }
            });
        }
    }
    let outline = if style.outline > 0 { dilate(&glyphs, wu, hu, style.outline as usize) } else { Vec::new() };

    out.clear();
    out.reserve(wu * hu * 4);
    for (i, &g) in glyphs.iter().enumerate() {
        let mut px = unpack(style.background, 1.0);
        if let Some(&o) = outline.get(i) {
            px = over(unpack(style.outline_color, o), px);
        }
        px = over(unpack(style.color, g), px);
        out.extend_from_slice(&pack(px));
    }
    Ok((width, height))
}

/// The coverage mask grown by `r` pixels (a disc).
fn dilate(mask: &[f32], w: usize, h: usize, r: usize) -> Vec<f32> {
    let offsets: Vec<(isize, isize, f32)> = (-(r as isize)..=r as isize)
        .flat_map(|dy| (-(r as isize)..=r as isize).map(move |dx| (dx, dy)))
        .filter_map(|(dx, dy)| {
            let d = ((dx * dx + dy * dy) as f32).sqrt();
            // Full strength out to the radius, with a one-pixel soft edge beyond it.
            let weight = (r as f32 + 1.0 - d).clamp(0.0, 1.0);
            (weight > 0.0).then_some((dx, dy, weight))
        })
        .collect();
    let mut out = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let c = mask[y * w + x];
            if c <= 0.0 {
                continue;
            }
            for &(dx, dy, wgt) in &offsets {
                let (nx, ny) = (x as isize + dx, y as isize + dy);
                if nx >= 0 && ny >= 0 && (nx as usize) < w && (ny as usize) < h {
                    let slot = &mut out[ny as usize * w + nx as usize];
                    *slot = slot.max(c * wgt);
                }
            }
        }
    }
    out
}

fn unpack(c: [u8; 4], coverage: f32) -> [f32; 4] {
    [f32::from(c[0]) / 255.0, f32::from(c[1]) / 255.0, f32::from(c[2]) / 255.0, f32::from(c[3]) / 255.0 * coverage]
}

/// `top` over `bottom`, both straight alpha.
fn over(top: [f32; 4], bottom: [f32; 4]) -> [f32; 4] {
    let a = top[3] + bottom[3] * (1.0 - top[3]);
    if a <= 0.0 {
        return [0.0; 4];
    }
    let mix = |i: usize| (top[i] * top[3] + bottom[i] * bottom[3] * (1.0 - top[3])) / a;
    [mix(0), mix(1), mix(2), a]
}

fn pack(c: [f32; 4]) -> [u8; 4] {
    c.map(|v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_blends_straight_alpha() {
        assert_eq!(pack(over([1.0, 0.0, 0.0, 1.0], [0.0, 0.0, 1.0, 1.0])), [255, 0, 0, 255]);
        assert_eq!(pack(over([1.0, 1.0, 1.0, 0.0], [0.0, 0.0, 0.0, 0.0])), [0, 0, 0, 0]);
        let half = pack(over([1.0, 1.0, 1.0, 0.5], [0.0, 0.0, 0.0, 1.0]));
        assert_eq!(half, [128, 128, 128, 255]);
    }

    #[test]
    fn dilation_grows_the_mask() {
        let mut m = vec![0.0; 25];
        m[12] = 1.0;
        let d = dilate(&m, 5, 5, 1);
        assert_eq!(d[12], 1.0);
        assert_eq!(d[7], 1.0);
        assert_eq!(d[0], 0.0);
    }

    #[test]
    fn renders_text_in_an_installed_font() {
        // Machines without fonts pass without checking.
        let Ok(font) = load_font("") else { return };
        let face = FontRef::try_from_slice_and_index(&font.bytes, font.index).unwrap();
        let mut out = Vec::new();
        let style = Style::read(&TextSource.schema().defaults().with("size", 40).with("outline", 2));
        let (w, h) = render(&face, "Hi\nthere", &style, &mut out).unwrap();
        assert_eq!(out.len(), (w * h * 4) as usize);
        assert!(w > 40 && h > 80, "{w} x {h}");
        let px = out.as_chunks::<4>().0;
        assert!(px.contains(&[255, 255, 255, 255]), "white glyph pixels");
        assert!(px.contains(&[0, 0, 0, 255]), "black outline pixels");
        assert!(px.iter().any(|p| p[3] == 0), "transparent background");
    }

    #[test]
    fn text_that_cannot_be_drawn_clears_the_earlier_text() {
        if load_font("").is_err() {
            return;
        }
        let io = SourceIo::new(studiocraft_media::clock::MasterClock::new());
        let small = TextSource.schema().defaults().with("text", "Hi").with("size", 40);
        let mut i = TextSource.create(&small, &io).unwrap();
        i.produce(&io).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Frame(_))));
        let huge = [&*"W".repeat(12); 8].join("\n");
        i.update(&small.clone().with("text", huge.as_str()).with("size", 1024), &io).unwrap();
        assert!(i.produce(&io).is_err());
        assert!(matches!(io.video.take(), Some(VideoContent::Empty)), "the earlier text is not shown as the current text");
    }

    #[test]
    fn text_larger_than_the_pixel_budget_is_refused() {
        let Ok(font) = load_font("") else { return };
        let face = FontRef::try_from_slice_and_index(&font.bytes, font.index).unwrap();
        let mut out = Vec::new();
        let style = Style::read(&TextSource.schema().defaults().with("size", 1024));
        let line = "W".repeat(12);
        let text = [line.as_str(); 8].join("\n");
        let e = render(&face, &text, &style, &mut out).unwrap_err();
        assert!(e.to_string().contains("smaller size"), "{e}");
        assert_eq!(out.capacity(), 0, "nothing was allocated for the picture");
    }
}
