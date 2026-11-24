//! The logos of the streaming services that allow StudioCraft to show them (AGENTS.md 1.2), and
//! how the Streaming window draws them.
//!
//! A logo is drawn only as its service's guidelines allow (`ASSETS.md`): whole, on a white tile,
//! with clear space around it, and never smaller than the least size the guidelines give. When a
//! destination's row is too small for that at the display's scale, the row shows the service's
//! badge and the destination's details show the logo at its full size. A service without a logo
//! StudioCraft may show has a badge everywhere: the first letter of its name on a tile tinted
//! from the theme, in a tone of its own.

use std::collections::BTreeMap;

use craft_theme::{Oklch, Rgba, ensure_contrast, over};
use egui::{Align2, Color32, CornerRadius, FontId, Rect, Stroke, StrokeKind, TextureHandle, Ui, pos2, vec2};
use studiocraft_model::services::{self, SERVICES, Service};

use crate::kit::corner;
use crate::theme;

/// The colors and corners of a logo tile: the outline of a logo's tile, the fill and mark of a
/// plain tile, and the corner radius of a tile on a destination's row and in its details. The
/// service tiles that add a destination also take the hover and pressed fills, the name's colors
/// and size, the focus ring and the padding from it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TileStyle {
    pub(crate) line: Color32,
    pub(crate) fill: Color32,
    pub(crate) mark: Color32,
    pub(crate) row_radius: u8,
    pub(crate) details_radius: u8,
    pub(crate) hover: Color32,
    pub(crate) pressed: Color32,
    pub(crate) text: Color32,
    pub(crate) text_disabled: Color32,
    /// The size of a service tile's name, in points.
    pub(crate) caption: f32,
    pub(crate) focus: Stroke,
    /// The space around a service tile's logo and name, in points.
    pub(crate) pad: f32,
    /// The color a badge's tone starts from: the soft accent over the panel.
    pub(crate) tint: Rgba,
    /// The color of a badge's letter before it is moved to read on the badge: the body text.
    pub(crate) ink: Rgba,
}

impl TileStyle {
    /// The style from the tokens of the window's root scope: the border line, the secondary
    /// control fills and text colors, the muted text color, the control and surface radii, the
    /// caption size, the focus ring, the regular padding, the soft accent over the panel and the
    /// body text.
    pub(crate) fn of(ctx: &egui::Context) -> TileStyle {
        theme::with_root(ctx, |cx| {
            let t = &cx.tokens;
            let look = cx.variant_look();
            TileStyle {
                line: theme::color(cx, t.color.line.border),
                fill: look.fill.off.rest.egui(),
                mark: theme::color(cx, t.color.text.muted),
                row_radius: corner(t.shape.radius.control),
                details_radius: corner(t.shape.radius.surface),
                hover: look.fill.off.hover.egui(),
                pressed: look.fill.off.pressed.egui(),
                text: look.text.off.rest.egui(),
                text_disabled: look.text.off.disabled.egui(),
                caption: t.typography.caption.size,
                focus: cx.focus_look().stroke,
                pad: t.space.pad.regular,
                tint: over(Rgba::from(theme::color(cx, t.color.accent.soft)), Rgba::from(theme::color(cx, t.color.surface.panel))),
                ink: Rgba::from(theme::color(cx, t.color.text.normal)),
            }
        })
    }

    /// The fill and letter colors of the badge of the service `id`.
    pub(crate) fn badge(&self, id: &str) -> (Color32, Color32) {
        let (fill, letter) = badge_tone(self.tint, self.ink, id);
        (egui_color(fill), egui_color(letter))
    }
}

/// The least contrast of a badge's letter on its fill (WCAG AA for text).
const BADGE_TEXT_MIN: f64 = 4.5;
/// The least and most OKLCH chroma of a badge's fill, so each tone stays a tint.
const BADGE_CHROMA: (f64, f64) = (0.04, 0.09);
/// The hue step between the badges of services next to each other in [`SERVICES`], in degrees.
const GOLDEN_ANGLE: f64 = 137.507_764;

/// The fill and letter colors of the badge of the service `id`: `tint` at its own lightness, its
/// chroma held to [`BADGE_CHROMA`] and its hue turned by [`hue_turn`], and `ink` moved only as far
/// as it needs to reach [`BADGE_TEXT_MIN`] on that fill.
fn badge_tone(tint: Rgba, ink: Rgba, id: &str) -> (Rgba, Rgba) {
    let base = tint.to_oklch();
    let fill = Oklch { l: base.l, c: base.c.clamp(BADGE_CHROMA.0, BADGE_CHROMA.1), h: (base.h + hue_turn(id)).rem_euclid(360.0) }.to_rgba();
    (fill, ensure_contrast(ink, fill, BADGE_TEXT_MIN))
}

/// How far the badge of the service `id` turns the tint's hue, in degrees: a golden-angle step
/// per place in [`SERVICES`], so services next to each other differ most, and a hash of the id
/// for a service this version does not list.
fn hue_turn(id: &str) -> f64 {
    match SERVICES.iter().position(|s| s.id == id) {
        Some(i) => (i as f64 * GOLDEN_ANGLE).rem_euclid(360.0),
        // FNV-1a, so the same id gets the same tone in every build.
        None => (id.bytes().fold(0x811c_9dc5_u32, |h, b| (h ^ u32::from(b)).wrapping_mul(0x0100_0193)) % 360) as f64,
    }
}

/// The letter a badge shows: the first letter or digit of `name`, in upper case; empty when the
/// name has none.
pub(crate) fn monogram(name: &str) -> String {
    name.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect()).unwrap_or_default()
}

/// `c` as egui paints it.
fn egui_color(c: Rgba) -> Color32 {
    let [r, g, b, a] = c.0;
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

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
/// The least height of the logo tile in a destination's details, and the side of a badge there,
/// in points.
pub(crate) const HEADER_TILE: f32 = 72.0;
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
    pub(crate) fn paint(&mut self, ui: &Ui, tile: Rect, logo: &'static Logo, s: &TileStyle, radius: u8) {
        let painter = ui.painter();
        painter.rect_filled(tile, CornerRadius::same(radius), Color32::WHITE);
        painter.rect_stroke(tile, CornerRadius::same(radius), Stroke::new(1.0, s.line), StrokeKind::Inside);
        let side = logo.side_in(tile.height());
        let ppp = ui.ctx().pixels_per_point();
        let Some(texture) = self.texture(ui.ctx(), logo, (side * ppp).round() as u32) else { return };
        let r = Rect::from_center_size(tile.center(), vec2(side * logo.aspect(), side));
        painter.image(texture, r, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
    }

    /// Paint the tile of a destination without a logo it can show here: the badge of the service
    /// `service` when it is one of [`SERVICES`], and a plain tile for a custom server.
    pub(crate) fn paint_plain(ui: &Ui, tile: Rect, service: &str, s: &TileStyle, radius: u8) {
        if let Some(svc) = services::service(service) {
            Logos::paint_badge(ui, tile, svc, s, radius);
            return;
        }
        let painter = ui.painter();
        painter.rect_filled(tile, CornerRadius::same(radius), s.fill);
        // A broadcast mark: a dot inside two rings.
        let c = tile.center();
        let unit = tile.height() / 10.0;
        painter.circle_filled(c, unit, s.mark);
        painter.circle_stroke(c, unit * 2.4, Stroke::new(1.5, s.mark));
        painter.circle_stroke(c, unit * 3.8, Stroke::new(1.5, s.mark.gamma_multiply(0.6)));
    }

    /// Paint the badge of `svc` in `tile`: its tone with the border line, and the first letter of
    /// its name centered at half the tile's height.
    pub(crate) fn paint_badge(ui: &Ui, tile: Rect, svc: &Service, s: &TileStyle, radius: u8) {
        let painter = ui.painter();
        let (fill, letter) = s.badge(svc.id);
        painter.rect_filled(tile, CornerRadius::same(radius), fill);
        painter.rect_stroke(tile, CornerRadius::same(radius), Stroke::new(1.0, s.line), StrokeKind::Inside);
        let font = FontId::new(tile.height() * 0.5, crate::fonts::semibold());
        painter.text(tile.center(), Align2::CENTER_CENTER, monogram(svc.name), font, letter);
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

    /// The smallest distance between two of `hues` around the color wheel, in degrees.
    fn least_gap(hues: &[f64]) -> f64 {
        let mut least = 360.0_f64;
        for (i, a) in hues.iter().enumerate() {
            for b in &hues[i + 1..] {
                let d = (a - b).rem_euclid(360.0);
                least = least.min(d.min(360.0 - d));
            }
        }
        least
    }

    #[test]
    fn a_badge_shows_the_first_letter_of_the_service_name() {
        let letters: Vec<(&str, String)> = ["kick", "x", "linkedin", "tiktok"]
            .iter()
            .map(|id| {
                let Some(svc) = services::service(id) else { panic!("no {id} preset") };
                (svc.id, monogram(svc.name))
            })
            .collect();
        assert_eq!(letters, [("kick", "K".to_string()), ("x", "X".into()), ("linkedin", "L".into()), ("tiktok", "T".into())]);
        for svc in SERVICES {
            assert_eq!(monogram(svc.name).chars().count(), 1, "{}", svc.id);
        }
        assert_eq!(monogram(""), "");
        assert_eq!(monogram("  -- "), "");
        assert_eq!(monogram(" (9) lives"), "9");
        assert_eq!(monogram("élan"), "É");
        assert_eq!(monogram("ßtream"), "SS", "a letter whose upper case is two letters keeps both");
    }

    #[test]
    fn each_service_has_a_tone_of_its_own_that_its_letter_reads_on() {
        let turns: Vec<f64> = SERVICES.iter().map(|s| hue_turn(s.id)).collect();
        assert!(least_gap(&turns) >= 30.0, "{turns:?}");
        for (a, b) in SERVICES.iter().zip(SERVICES.iter().skip(1)) {
            let d = (hue_turn(a.id) - hue_turn(b.id)).rem_euclid(360.0);
            assert!(d.min(360.0 - d) >= 90.0, "{} and {} stand next to each other", a.id, b.id);
        }
        // A service this version does not list gets the same tone every time.
        assert_eq!(hue_turn("future"), hue_turn("future"));
        assert!((0.0..360.0).contains(&hue_turn("future")) && (0.0..360.0).contains(&hue_turn("")));
        // Light and dark tints, a grey one and a loud one: every fill stays a tint, and every
        // letter reaches the text minimum on its fill.
        let tints = [Rgba::rgb(0xe6, 0xee, 0xfb), Rgba::rgb(0x1f, 0x2d, 0x45), Rgba::rgb(0x80, 0x80, 0x80), Rgba::rgb(0xff, 0x00, 0x40)];
        let inks = [Rgba::rgb(0x1a, 0x1a, 0x1a), Rgba::rgb(0xf2, 0xf2, 0xf2), Rgba::rgb(0x80, 0x80, 0x80)];
        for tint in tints {
            for ink in inks {
                let mut hues = Vec::new();
                for svc in SERVICES {
                    let (fill, letter) = badge_tone(tint, ink, svc.id);
                    let f = fill.to_oklch();
                    assert!(f.c <= BADGE_CHROMA.1 + 0.005, "{} on {tint:?}: chroma {}", svc.id, f.c);
                    assert!((f.l - tint.to_oklch().l).abs() < 0.02, "{} keeps the tint's lightness", svc.id);
                    let ratio = craft_theme::contrast_ratio(letter, fill);
                    assert!(ratio >= BADGE_TEXT_MIN, "{} on {tint:?} with {ink:?}: {ratio:.2}:1", svc.id);
                    hues.push(f.h);
                }
                assert!(least_gap(&hues) >= 25.0, "{tint:?}: {hues:?}");
            }
        }
        // A letter that already reads on the fill is kept as it is.
        let (fill, letter) = badge_tone(tints[0], inks[0], "kick");
        assert!(craft_theme::contrast_ratio(inks[0], fill) >= BADGE_TEXT_MIN);
        assert_eq!(letter, inks[0]);
    }

    #[test]
    fn the_badges_take_their_tones_from_both_suite_modes() {
        use craft_theme::Mode;
        use craft_ui::runtime::frame::FrameInput;
        let mut seen = Vec::new();
        for mode in [Mode::Light, Mode::Dark] {
            let ctx = crate::test_context();
            craft_ui::shell::embed::before_input(&ctx, |_| FrameInput::new(crate::theme::root_env().mode(mode)));
            let mut style = None;
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| style = Some(TileStyle::of(ui.ctx())));
            out.textures_delta.clear();
            let Some(style) = style else { panic!("the frame did not run") };
            let badges: Vec<(Color32, Color32)> = SERVICES.iter().map(|s| style.badge(s.id)).collect();
            for (svc, (fill, letter)) in SERVICES.iter().zip(&badges) {
                let ratio = craft_theme::contrast_ratio(Rgba::from(*letter), Rgba::from(*fill));
                assert!(ratio >= BADGE_TEXT_MIN, "{mode:?}: {} is {ratio:.2}:1", svc.id);
                assert_eq!(fill.a(), 255, "{mode:?}: {} is opaque", svc.id);
            }
            for (i, a) in badges.iter().enumerate() {
                assert!(badges[i + 1..].iter().all(|b| b.0 != a.0), "{mode:?}: {} shares its tone", SERVICES[i].id);
            }
            seen.push(badges[0].0);
        }
        assert_ne!(seen[0], seen[1], "each mode tints its badges from its own theme");
    }
}
