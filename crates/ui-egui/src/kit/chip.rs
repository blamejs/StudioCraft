//! A small text label on a solid fill, drawn over other art, such as the program and preview marks
//! on a scene tile.
//!
//! A chip is as tall as its style says and as wide as its text plus its padding. Its text is in
//! the semibold face and is never shortened, so a chip carries a short word. The fill and text
//! colors come from the caller's style, so the text and not the color alone says what the chip
//! means.

use craft_ui::style::StyleCx;
use egui::{Color32, CornerRadius, FontFamily, FontId, Painter, Pos2, Rect, pos2, vec2};

use super::corner;

/// The colors and sizes of a [`Chip`].
#[derive(Clone, Debug, PartialEq)]
pub struct ChipStyle {
    pub fill: Color32,
    pub text: Color32,
    pub height: f32,
    /// The space between each end and the text.
    pub padding: f32,
    pub radius: u8,
    pub font: FontId,
}

impl ChipStyle {
    /// A chip on the theme's error solid with the text for a status fill: the program mark.
    pub fn on_air(cx: &StyleCx<'_>, family: FontFamily) -> ChipStyle {
        let t = &cx.tokens;
        ChipStyle::with(cx, cx.color(t.color.status.bad.solid).egui(), cx.color(t.color.text.on_status).egui(), family)
    }

    /// A chip on the theme's accent solid with the text for an accent fill: the preview mark.
    pub fn accent(cx: &StyleCx<'_>, family: FontFamily) -> ChipStyle {
        let t = &cx.tokens;
        ChipStyle::with(cx, cx.color(t.color.accent.solid).egui(), cx.color(t.color.text.on_accent).egui(), family)
    }

    /// A chip filled with `fill` and its text in `text`, 16 points tall, with the caption size in
    /// `family` (the semibold face) and the small radius.
    pub fn with(cx: &StyleCx<'_>, fill: Color32, text: Color32, family: FontFamily) -> ChipStyle {
        let t = &cx.tokens;
        ChipStyle { fill, text, height: 16.0, padding: 4.0, radius: corner(t.shape.radius.sm), font: FontId::new(t.typography.caption.size, family) }
    }
}

/// One chip. The text is shown as given, already translated.
pub struct Chip<'a> {
    text: &'a str,
    style: &'a ChipStyle,
}

impl<'a> Chip<'a> {
    pub fn new(text: &'a str, style: &'a ChipStyle) -> Chip<'a> {
        Chip { text, style }
    }

    /// The chip's width for its text.
    pub fn width(&self, painter: &Painter) -> f32 {
        let g = painter.layout_no_wrap(self.text.to_owned(), self.style.font.clone(), self.style.text);
        g.size().x.ceil() + 2.0 * self.style.padding
    }

    /// Draw the chip with its top-left corner at `at`; returns where it was drawn.
    pub fn paint(&self, painter: &Painter, at: Pos2) -> Rect {
        let s = self.style;
        let g = painter.layout_no_wrap(self.text.to_owned(), s.font.clone(), s.text);
        let rect = Rect::from_min_size(at, vec2(g.size().x.ceil() + 2.0 * s.padding, s.height));
        painter.rect_filled(rect, CornerRadius::same(s.radius), s.fill);
        painter.galley(pos2(rect.center().x - g.size().x / 2.0, rect.center().y - g.size().y / 2.0), g, s.text);
        rect
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styles() -> (ChipStyle, ChipStyle) {
        StyleCx::with_root(&crate::test_context(), |cx| {
            (ChipStyle::on_air(cx, FontFamily::Proportional), ChipStyle::accent(cx, FontFamily::Proportional))
        })
    }

    #[test]
    fn the_program_chip_is_the_error_solid_and_the_preview_chip_the_accent_solid() {
        let ctx = crate::test_context();
        let (bad, accent, on_status, on_accent) = StyleCx::with_root(&ctx, |cx| {
            let t = &cx.tokens;
            (
                cx.color(t.color.status.bad.solid).egui(),
                cx.color(t.color.accent.solid).egui(),
                cx.color(t.color.text.on_status).egui(),
                cx.color(t.color.text.on_accent).egui(),
            )
        });
        let (pgm, pvw) = styles();
        assert_eq!((pgm.fill, pgm.text), (bad, on_status));
        assert_eq!((pvw.fill, pvw.text), (accent, on_accent));
        assert_eq!(pgm.height, 16.0);
    }

    #[test]
    fn a_chip_fits_its_text_and_draws_its_fill_where_it_says() {
        let ctx = egui::Context::default();
        let (pgm, _) = styles();
        let mut drawn = (Rect::NOTHING, 0.0);
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let chip = Chip::new("PGM", &pgm);
            let w = chip.width(ui.painter());
            drawn = (chip.paint(ui.painter(), pos2(10.0, 20.0)), w);
        });
        out.textures_delta.clear();
        let (rect, width) = drawn;
        assert_eq!(rect.min, pos2(10.0, 20.0));
        assert_eq!((rect.width(), rect.height()), (width, 16.0));
        assert!(width > 2.0 * pgm.padding, "the text takes room: {width}");
        assert!(out.shapes.iter().any(|c| matches!(&c.shape, egui::Shape::Rect(s) if s.fill == pgm.fill && s.rect == rect)));
    }
}
