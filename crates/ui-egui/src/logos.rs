//! The logos of the streaming services that allow StudioCraft to show them (AGENTS.md 1.2), and
//! how the Streaming window draws them.
//!
//! A logo is drawn only as its service's guidelines allow (`ASSETS.md`): whole, on a white tile,
//! with clear space around it, and never smaller than the least size the guidelines give. When a
//! destination's row is too small for that at the display's scale, the row shows a plain tile and
//! the destination's details show the logo at its full size.

use std::collections::BTreeMap;

use egui::{Color32, CornerRadius, Rect, Stroke, StrokeKind, TextureHandle, Ui, pos2, vec2};

use crate::kit::tokens::Palette;

/// A service logo file and the rules its guidelines set for drawing it.
pub struct Logo {
    /// The id of the service in [`studiocraft_model::SERVICES`].
    pub service: &'static str,
    pub png: &'static [u8],
    /// The least height the whole file may be drawn at, in screen pixels.
    pub min_px: f32,
    /// The clear space kept on each side of the file, as a share of its drawn height; 0 when the
    /// file carries its clear space in its margins.
    pub clear: f32,
    /// Whether text may be drawn beside the logo. When it may not, the logo is drawn only in the
    /// destination's details, on a row of its own.
    pub beside_text: bool,
}

/// The service logos StudioCraft has (`assets/platform-logos`). The other services have none.
pub const LOGOS: &[Logo] = &[
    // The guidelines give no least size; 16 px keeps the mark legible.
    Logo {
        service: "twitch",
        png: include_bytes!("../../../assets/platform-logos/twitch-glitch-purple.png"),
        min_px: 16.0,
        clear: 0.25,
        beside_text: true,
    },
    // The icon must be at least 100 px tall. It spans 577 of the file's 1075 rows, and the
    // margins are its clear space.
    Logo {
        service: "youtube",
        png: include_bytes!("../../../assets/platform-logos/youtube-icon-red.png"),
        min_px: 187.0,
        clear: 0.0,
        beside_text: true,
    },
    // At least 16 px wide, a quarter of its width clear on each side, and no word beside it.
    Logo {
        service: "facebook",
        png: include_bytes!("../../../assets/platform-logos/facebook-logo-primary.png"),
        min_px: 16.0,
        clear: 0.25,
        beside_text: false,
    },
];

/// The height of the tile in a destination's row, in points.
pub const ROW_TILE: f32 = 28.0;
/// The least height of the logo tile in a destination's details, in points.
const HEADER_TILE: f32 = 72.0;
/// The largest side a logo texture is decoded at, in pixels.
const MAX_TEXTURE: u32 = 1024;

/// The logo of the service `id`, when it has one.
pub fn logo(id: &str) -> Option<&'static Logo> {
    LOGOS.iter().find(|l| l.service == id)
}

impl Logo {
    /// The drawn height of the file in a tile `tile` points tall, in points.
    fn side_in(&self, tile: f32) -> f32 {
        tile / (1.0 + 2.0 * self.clear)
    }

    /// Whether a row tile `tile` points tall can show the logo at `pixels_per_point`.
    pub fn fits_row(&self, tile: f32, pixels_per_point: f32) -> bool {
        let side = self.side_in(tile);
        self.beside_text && side * pixels_per_point >= self.min_px && side * (1.0 + 2.0 * self.clear) * self.aspect() <= tile
    }

    /// The tile height of the logo in a destination's details, in points: at least
    /// `HEADER_TILE`, and tall enough for the file's least size.
    pub fn header_tile(&self, pixels_per_point: f32) -> f32 {
        let least = self.min_px / pixels_per_point.max(0.1) * (1.0 + 2.0 * self.clear);
        least.max(HEADER_TILE).ceil()
    }

    /// The width of the details' tile `tile` points tall, in points.
    pub fn header_width(&self, tile: f32) -> f32 {
        (self.side_in(tile) * self.aspect() + (tile - self.side_in(tile))).max(tile)
    }

    /// The space kept below the details' tile `tile` points tall, in points: half the logo's
    /// height.
    pub fn header_gap(&self, tile: f32) -> f32 {
        self.side_in(tile) * 0.5
    }

    /// The file's width over its height.
    pub fn aspect(&self) -> f32 {
        png_size(self.png).filter(|(_, h)| *h > 0).map_or(1.0, |(w, h)| w as f32 / h as f32)
    }
}

/// The width and height a PNG file gives in its header; `None` when it is not a PNG.
fn png_size(png: &[u8]) -> Option<(u32, u32)> {
    let header = png.get(..24)?;
    if header[..8] != [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] || &header[12..16] != b"IHDR" {
        return None;
    }
    let word = |at: usize| u32::from_be_bytes([header[at], header[at + 1], header[at + 2], header[at + 3]]);
    Some((word(16), word(20)))
}

/// Decode `png` at a height of at most `height` pixels, averaging blocks of pixels when it is
/// larger.
fn decode(png: &[u8], height: u32) -> Option<egui::ColorImage> {
    let icon = eframe::icon_data::from_png_bytes(png).ok()?;
    let (w, h) = (icon.width as usize, icon.height as usize);
    if w == 0 || h == 0 || icon.rgba.len() != w * h * 4 {
        return None;
    }
    // At least `height` rows, so the window only ever scales the texture down, and at most
    // `MAX_TEXTURE`.
    let target = height.clamp(1, MAX_TEXTURE) as usize;
    let f = (h / target).max(h.div_ceil(MAX_TEXTURE as usize)).max(1);
    let (ow, oh) = (w / f, h / f);
    if f == 1 || ow == 0 || oh == 0 {
        return Some(egui::ColorImage::from_rgba_unmultiplied([w, h], &icon.rgba));
    }
    let mut out = vec![0u8; ow * oh * 4];
    for oy in 0..oh {
        for ox in 0..ow {
            // Weight each color by its alpha so transparent pixels do not darken the edges.
            let (mut r, mut g, mut b, mut a) = (0u64, 0u64, 0u64, 0u64);
            for y in oy * f..oy * f + f {
                for x in ox * f..ox * f + f {
                    let Some(px) = icon.rgba.get((y * w + x) * 4..(y * w + x) * 4 + 4) else { continue };
                    let alpha = u64::from(px[3]);
                    r += u64::from(px[0]) * alpha;
                    g += u64::from(px[1]) * alpha;
                    b += u64::from(px[2]) * alpha;
                    a += alpha;
                }
            }
            let o = (oy * ow + ox) * 4;
            let mean = |sum: u64| sum.checked_div(a).unwrap_or(0) as u8;
            if let Some(px) = out.get_mut(o..o + 4) {
                px.copy_from_slice(&[mean(r), mean(g), mean(b), (a / (f * f) as u64) as u8]);
            }
        }
    }
    Some(egui::ColorImage::from_rgba_unmultiplied([ow, oh], &out))
}

/// The decoded logo textures, by service id and height in pixels.
#[derive(Default)]
pub(crate) struct Logos {
    textures: BTreeMap<(&'static str, u32), Option<TextureHandle>>,
}

impl Logos {
    /// The texture of `logo` for drawing it `height` pixels tall.
    fn texture(&mut self, ctx: &egui::Context, logo: &'static Logo, height: u32) -> Option<egui::TextureId> {
        let key = (logo.service, height);
        let slot = self.textures.entry(key).or_insert_with(|| {
            decode(logo.png, height).map(|img| ctx.load_texture(format!("logo-{}-{height}", logo.service), img, egui::TextureOptions::LINEAR))
        });
        slot.as_ref().map(TextureHandle::id)
    }

    /// Paint `logo` in `tile`: a white rounded tile with the logo centered, keeping its clear space.
    pub(crate) fn paint(&mut self, ui: &Ui, tile: Rect, logo: &'static Logo, p: &Palette, radius: u8) {
        let painter = ui.painter();
        painter.rect_filled(tile, CornerRadius::same(radius), Color32::WHITE);
        painter.rect_stroke(tile, CornerRadius::same(radius), Stroke::new(1.0, p.line), StrokeKind::Inside);
        let side = logo.side_in(tile.height());
        let ppp = ui.ctx().pixels_per_point();
        let Some(texture) = self.texture(ui.ctx(), logo, (side * ppp).round() as u32) else { return };
        let r = Rect::from_center_size(tile.center(), vec2(side * logo.aspect(), side));
        painter.image(texture, r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
    }

    /// Paint the plain tile of a destination without a logo it can show here.
    pub(crate) fn paint_plain(ui: &Ui, tile: Rect, p: &Palette, radius: u8) {
        let painter = ui.painter();
        painter.rect_filled(tile, CornerRadius::same(radius), p.control);
        // A broadcast mark: a dot inside two rings.
        let c = tile.center();
        let unit = tile.height() / 10.0;
        painter.circle_filled(c, unit, p.text_muted);
        painter.circle_stroke(c, unit * 2.4, Stroke::new(1.5, p.text_muted));
        painter.circle_stroke(c, unit * 3.8, Stroke::new(1.5, p.text_muted.gamma_multiply(0.6)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use studiocraft_model::services;

    #[test]
    fn every_logo_decodes_and_belongs_to_a_preset_service() {
        for l in LOGOS {
            let Some(svc) = services::service(l.service) else { panic!("{} has no preset", l.service) };
            let (w, h) = png_size(l.png).unwrap_or_else(|| panic!("{} is not a PNG", svc.id));
            let full = decode(l.png, h).unwrap_or_else(|| panic!("{} does not decode", svc.id));
            if h <= MAX_TEXTURE {
                assert_eq!(full.size, [w as usize, h as usize], "{}: drawn from the file as it is", svc.id);
            } else {
                assert!(full.size[1] <= MAX_TEXTURE as usize && full.size[1] * 2 > MAX_TEXTURE as usize, "{}: {:?}", svc.id, full.size);
            }
            let small = decode(l.png, 64).unwrap_or_else(|| panic!("{} does not decode small", svc.id));
            assert!(small.size[1] >= 64 && small.size[1] < 128, "{}: {:?}", svc.id, small.size);
            assert!(l.min_px >= 16.0, "{}", svc.id);
        }
        let with_logo: Vec<&str> = LOGOS.iter().map(|l| l.service).collect();
        assert_eq!(with_logo, ["twitch", "youtube", "facebook"]);
        for id in ["kick", "x", "linkedin", "tiktok"] {
            assert!(services::service(id).is_some() && logo(id).is_none(), "{id} has no logo");
        }
        assert_eq!(png_size(b"not a png file at all, no"), None);
    }

    #[test]
    fn a_logo_is_drawn_no_smaller_than_its_guidelines_allow() {
        let Some(twitch) = logo("twitch") else { panic!("no twitch logo") };
        let Some(youtube) = logo("youtube") else { panic!("no youtube logo") };
        let Some(facebook) = logo("facebook") else { panic!("no facebook logo") };
        assert!(twitch.fits_row(ROW_TILE, 1.0) && twitch.fits_row(ROW_TILE, 2.0));
        // At half a pixel per point the mark would be 9 px tall, under its 16 px.
        assert!(!twitch.fits_row(ROW_TILE, 0.5));
        // 100 px of icon needs 187 px of the file: a row shows a plain tile at every usual scale.
        for ppp in [1.0, 1.25, 1.5, 2.0, 3.0] {
            assert!(!youtube.fits_row(ROW_TILE, ppp), "{ppp}");
        }
        // No word may stand beside this logo, so it is drawn only in the details.
        assert!(!facebook.fits_row(ROW_TILE, 2.0));
        for l in LOGOS {
            for ppp in [0.5, 1.0, 1.5, 2.0, 4.0] {
                let tile = l.header_tile(ppp);
                assert!(l.side_in(tile) * ppp >= l.min_px - 0.5, "{} at {ppp}: {tile}", l.service);
                assert!(tile >= HEADER_TILE);
                assert!(l.header_width(tile) >= tile);
            }
        }
        assert_eq!(youtube.header_tile(1.0), 187.0);
    }
}
