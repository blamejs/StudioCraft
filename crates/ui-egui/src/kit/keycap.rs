//! A shortcut drawn as one small raised key, such as the shortcut that shows a scene, drawn over
//! a scene tile's picture.
//!
//! The key has the theme's raised surface, a 1-point border in the theme's border color and the
//! shortcut's text in the normal text color, in the caption size. The caller writes the text in
//! the platform's notation (`Ctrl+1`, `⌘1`). A key wider than the room it is given is not drawn,
//! so a shortcut is never shown cut off.

use craft_ui::style::StyleCx;
use egui::{Color32, CornerRadius, FontFamily, FontId, Painter, Pos2, Rect, Stroke, StrokeKind, pos2, vec2};

use super::corner;

/// The colors and sizes of a [`Keycap`].
#[derive(Clone, Debug, PartialEq)]
pub struct KeycapStyle {
    pub fill: Color32,
    pub border: Color32,
    pub text: Color32,
    pub height: f32,
    /// The space between each side and the text.
    pub padding: f32,
    pub radius: u8,
    pub font: FontId,
}

impl KeycapStyle {
    /// The style from the tokens of `cx`: the raised surface, the border line and the normal
    /// text, 16 points tall with the small radius, in the caption size of `family`.
    pub fn new(cx: &StyleCx<'_>, family: FontFamily) -> KeycapStyle {
        let t = &cx.tokens;
        KeycapStyle {
            fill: cx.color(t.color.surface.raised).egui(),
            border: cx.color(t.color.line.border).egui(),
            text: cx.color(t.color.text.normal).egui(),
            height: 16.0,
            padding: 4.0,
            radius: corner(t.shape.radius.sm),
            font: FontId::new(t.typography.caption.size, family),
        }
    }
}

/// One key. The text is shown as given.
pub struct Keycap<'a> {
    text: &'a str,
    style: &'a KeycapStyle,
}

impl<'a> Keycap<'a> {
    pub fn new(text: &'a str, style: &'a KeycapStyle) -> Keycap<'a> {
        Keycap { text, style }
    }

    /// The key's width for its text.
    pub fn width(&self, painter: &Painter) -> f32 {
        let g = painter.layout_no_wrap(self.text.to_owned(), self.style.font.clone(), self.style.text);
        g.size().x.ceil() + 2.0 * self.style.padding
    }

    /// Draw the key with its top-left corner at `at`, when it is no wider than `room`; returns
    /// where it was drawn, or `None` when it was too wide or has no text.
    pub fn paint(&self, painter: &Painter, at: Pos2, room: f32) -> Option<Rect> {
        let s = self.style;
        if self.text.is_empty() {
            return None;
        }
        let g = painter.layout_no_wrap(self.text.to_owned(), s.font.clone(), s.text);
        let width = g.size().x.ceil() + 2.0 * s.padding;
        if room.is_nan() || width > room {
            return None;
        }
        let rect = Rect::from_min_size(at, vec2(width, s.height));
        let radius = CornerRadius::same(s.radius);
        painter.rect_filled(rect, radius, s.fill);
        painter.rect_stroke(rect, radius, Stroke::new(1.0, s.border), StrokeKind::Inside);
        painter.galley(pos2(rect.center().x - g.size().x / 2.0, rect.center().y - g.size().y / 2.0), g, s.text);
        Some(rect)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style() -> KeycapStyle {
        StyleCx::with_root(&crate::test_context(), |cx| KeycapStyle::new(cx, FontFamily::Proportional))
    }

    fn paint(text: &str, room: f32) -> (Option<Rect>, egui::FullOutput) {
        let ctx = egui::Context::default();
        let st = style();
        let mut drawn = None;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| drawn = Keycap::new(text, &st).paint(ui.painter(), pos2(4.0, 4.0), room));
        out.textures_delta.clear();
        (drawn, out)
    }

    #[test]
    fn a_key_is_drawn_on_the_raised_surface_at_its_corner() {
        let st = style();
        let (drawn, out) = paint("Ctrl+1", 200.0);
        let Some(rect) = drawn else { panic!("the key was not drawn") };
        assert_eq!((rect.min, rect.height()), (pos2(4.0, 4.0), 16.0));
        assert!(out.shapes.iter().any(|c| matches!(&c.shape, egui::Shape::Rect(s) if s.fill == st.fill && s.rect == rect)));
    }

    #[test]
    fn a_key_wider_than_its_room_or_without_text_is_not_drawn() {
        assert_eq!(paint("Ctrl+Shift+Alt+9", 20.0).0, None);
        assert_eq!(paint("", 200.0).0, None);
        assert_eq!(paint("1", f32::NAN).0, None, "a broken room draws nothing");
    }
}
