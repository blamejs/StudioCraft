//! A warning strip across the available width: a pause glyph and a text at the start, and buttons
//! at the end, or the other way round in a right-to-left interface.
//!
//! The strip is filled with the theme's warning solid and draws its glyph, text and buttons in the
//! color for text on a status fill, so the text and the glyph carry the state as well as the color. The text is
//! shortened with an ellipsis when the buttons leave it too little room, and on a strip narrower
//! than its buttons the buttons share the width equally with shortened labels. The strip is a polite
//! live region for screen readers: it announces its `announce` text, which the caller changes only
//! at the moments worth announcing, and not the visible text, which may change every second. The
//! strip appears at once, with no fade.

use craft_ui::style::StyleCx;
use egui::text::{LayoutJob, TextWrapping};
use egui::{Color32, CornerRadius, FontId, Rect, Response, Role, Sense, Stroke, StrokeKind, TextFormat, TextStyle, Ui, WidgetInfo, pos2, vec2};

use super::corner;
use super::onair::{Glyph, paint_glyph};

/// The colors and sizes of a [`Banner`].
#[derive(Clone, Debug, PartialEq)]
pub struct BannerStyle {
    pub fill: Color32,
    /// The glyph, the text and the buttons' outlines and labels.
    pub on_fill: Color32,
    pub height: f32,
    /// The height of a button.
    pub button_height: f32,
    /// The space between the edges, the glyph, the text and the buttons.
    pub gap: f32,
    /// The space inside a button on each side of its label.
    pub button_padding: f32,
    pub radius: u8,
    pub font: FontId,
}

impl BannerStyle {
    /// The style from the tokens of `cx`: the warning solid, the standard control height and the
    /// control radius, 30 points tall, with its text in `font`.
    pub fn new(cx: &StyleCx<'_>, font: FontId) -> BannerStyle {
        let t = &cx.tokens;
        BannerStyle {
            fill: cx.color(t.color.status.warn.solid).egui(),
            on_fill: cx.color(t.color.text.on_status).egui(),
            height: 30.0,
            button_height: (t.size.control.md - 2.0).max(20.0),
            gap: 8.0,
            button_padding: 8.0,
            radius: corner(t.shape.radius.control),
            font,
        }
    }
}

/// One button of a banner: the label it shows and the name a screen reader announces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BannerAction<'a> {
    pub label: &'a str,
    pub name: &'a str,
}

/// What [`Banner::show`] returns.
pub struct BannerResponse {
    /// The whole strip.
    pub rect: Rect,
    /// Each button's response, in the order the actions were given.
    pub actions: Vec<Response>,
    /// Where the glyph was drawn; `None` when the buttons left no room for it.
    pub glyph: Option<Rect>,
    /// The text did not fit and was shortened or left out.
    pub truncated: bool,
}

impl BannerResponse {
    /// The index of the button clicked this frame.
    pub fn clicked(&self) -> Option<usize> {
        self.actions.iter().position(Response::clicked)
    }
}

/// A warning strip. Texts are shown as given, already translated.
pub struct Banner<'a> {
    text: &'a str,
    announce: &'a str,
    actions: &'a [BannerAction<'a>],
    style: &'a BannerStyle,
}

impl<'a> Banner<'a> {
    /// A strip showing `text`, announcing `announce` to screen readers, with `actions` as buttons
    /// in that order from the start; the last one is filled.
    pub fn new(text: &'a str, announce: &'a str, actions: &'a [BannerAction<'a>], style: &'a BannerStyle) -> Banner<'a> {
        Banner { text, announce, actions, style }
    }

    pub fn show(self, ui: &mut Ui) -> BannerResponse {
        let s = self.style;
        let rtl = craft_ui::runtime::env::Env::of(ui).view().direction().is_rtl();
        let (rect, strip) = ui.allocate_exact_size(vec2(ui.available_width(), s.height), Sense::hover());
        let live = self.announce.to_owned();
        ui.ctx().accesskit_node_builder(strip.id, |node| {
            node.set_role(Role::Status);
            node.set_live(egui::accesskit::Live::Polite);
            node.set_label(live);
        });
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, CornerRadius::same(s.radius), s.fill);
        // The end edge, and the direction from the end edge toward the start.
        let (end, back) = if rtl { (rect.left() + s.gap, 1.0) } else { (rect.right() - s.gap, -1.0) };
        let button_font = TextStyle::Button.resolve(ui.style());
        let mut widths: Vec<f32> = self
            .actions
            .iter()
            .map(|a| painter.layout_no_wrap(a.label.to_owned(), button_font.clone(), s.on_fill).size().x.ceil() + 2.0 * s.button_padding)
            .collect();
        // When the buttons do not fit between the edges, they share the room equally and their
        // labels are shortened.
        let count = widths.len() as f32;
        let room = (rect.width() - (count + 1.0) * s.gap).max(0.0);
        if widths.iter().sum::<f32>() > room {
            widths.fill(room / count.max(1.0));
        }
        let mut taken = 0.0;
        let mut boxes = vec![Rect::NOTHING; self.actions.len()];
        for (i, w) in widths.iter().enumerate().rev() {
            let near = end + back * taken;
            let far = near + back * w;
            boxes[i] = Rect::from_x_y_ranges(
                near.min(far)..=near.max(far),
                rect.center().y - s.button_height / 2.0..=rect.center().y + s.button_height / 2.0,
            );
            taken += w + s.gap;
        }
        let mut actions = Vec::with_capacity(self.actions.len());
        let last = self.actions.len().saturating_sub(1);
        for (i, (a, b)) in self.actions.iter().zip(&boxes).enumerate() {
            let r = ui.interact(*b, strip.id.with(("action", i)), Sense::click());
            r.widget_info(|| WidgetInfo::labeled(Role::Button, true, a.name));
            let radius = CornerRadius::same(s.radius);
            let pressed = r.is_pointer_button_down_on();
            let (fill, text) = if i == last { (s.on_fill, s.fill) } else { (Color32::TRANSPARENT, s.on_fill) };
            let fill = if pressed && fill != Color32::TRANSPARENT { fill.gamma_multiply(0.85) } else { fill };
            painter.rect_filled(*b, radius, if r.hovered() && i != last { s.on_fill.gamma_multiply(0.12) } else { fill });
            painter.rect_stroke(*b, radius, Stroke::new(1.0, s.on_fill), StrokeKind::Inside);
            let mut job = LayoutJob::single_section(a.label.to_owned(), TextFormat::simple(button_font.clone(), text));
            job.wrap = TextWrapping::truncate_at_width((b.width() - 2.0 * s.button_padding).max(0.0));
            let g = painter.layout_job(job);
            painter.galley(b.center() - g.size() / 2.0, g, text);
            actions.push(r);
        }
        // The glyph and the text from the start edge, in the room the buttons leave; each is left
        // out when that room cannot hold it.
        let glyph_size = 12.0;
        let start = if rtl { rect.right() - s.gap } else { rect.left() + s.gap };
        let fwd = -back;
        let text_end = end + back * taken;
        let free = (text_end - start) * fwd;
        if free < glyph_size {
            return BannerResponse { rect, actions, glyph: None, truncated: true };
        }
        let glyph_far = start + fwd * glyph_size;
        let glyph = Rect::from_x_y_ranges(
            start.min(glyph_far)..=start.max(glyph_far),
            rect.center().y - glyph_size / 2.0..=rect.center().y + glyph_size / 2.0,
        );
        paint_glyph(&painter, Glyph::Pause, glyph, s.on_fill, true);
        let text_start = glyph_far + fwd * s.gap;
        let room = (text_end - text_start) * fwd;
        let ellipsis = painter.layout_no_wrap("…".to_owned(), s.font.clone(), s.on_fill).size().x;
        if room < ellipsis {
            return BannerResponse { rect, actions, glyph: Some(glyph), truncated: true };
        }
        let mut job = LayoutJob::single_section(self.text.to_owned(), TextFormat::simple(s.font.clone(), s.on_fill));
        job.wrap = TextWrapping::truncate_at_width(room);
        let g = painter.layout_job(job);
        let truncated = g.elided;
        let x = if rtl { text_start - g.size().x } else { text_start };
        painter.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, s.on_fill);
        BannerResponse { rect, actions, glyph: Some(glyph), truncated }
    }
}

#[cfg(test)]
mod tests {
    use egui::{Event, Modifiers, PointerButton, RawInput};

    use super::*;

    const ACTIONS: [BannerAction<'static>; 2] =
        [BannerAction { label: "+1 min", name: "Add a Minute to the Pause" }, BannerAction { label: "Resume", name: "Resume Program" }];

    fn style() -> BannerStyle {
        StyleCx::with_root(&crate::test_context(), |cx| BannerStyle::new(cx, FontId::proportional(12.0)))
    }

    fn raw(width: f32, events: Vec<Event>) -> RawInput {
        RawInput { events, screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(width, 100.0))), ..RawInput::default() }
    }

    fn draw(ctx: &egui::Context, width: f32, events: Vec<Event>, text: &str) -> (BannerResponse, egui::FullOutput) {
        let st = style();
        let mut resp = None;
        let mut out = ctx.run_ui(raw(width, events), |ui| resp = Some(Banner::new(text, "Paused", &ACTIONS, &st).show(ui)));
        out.textures_delta.clear();
        let Some(resp) = resp else { panic!("the banner was not drawn") };
        (resp, out)
    }

    fn click(at: egui::Pos2) -> Vec<Event> {
        let b = |pressed| Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        vec![Event::PointerMoved(at), b(true), b(false)]
    }

    #[test]
    fn the_banner_has_the_warning_fill_and_its_buttons_sit_at_the_end_in_order() {
        let ctx = egui::Context::default();
        let (r, out) = draw(&ctx, 800.0, vec![], "Paused.");
        assert_eq!(r.rect.height(), 30.0);
        let warn = style().fill;
        assert!(out.shapes.iter().any(|c| matches!(&c.shape, egui::Shape::Rect(s) if s.fill == warn && s.rect == r.rect)));
        let [more, resume] = [r.actions[0].rect, r.actions[1].rect];
        assert!(more.right() < resume.left(), "+1 min comes before Resume: {more:?} {resume:?}");
        assert!(resume.right() <= r.rect.right() && resume.right() > r.rect.right() - 20.0, "{resume:?}");
        assert!(!r.truncated);
    }

    #[test]
    fn each_button_reports_its_own_click() {
        let ctx = egui::Context::default();
        let (r, _) = draw(&ctx, 800.0, vec![], "Paused.");
        let (more, resume) = (r.actions[0].rect.center(), r.actions[1].rect.center());
        let (r, _) = draw(&ctx, 800.0, click(more), "Paused.");
        assert_eq!(r.clicked(), Some(0));
        let (r, _) = draw(&ctx, 800.0, click(resume), "Paused.");
        assert_eq!(r.clicked(), Some(1));
        let (r, _) = draw(&ctx, 800.0, vec![], "Paused.");
        assert_eq!(r.clicked(), None);
    }

    #[test]
    fn a_narrow_banner_shortens_its_text_and_keeps_its_buttons() {
        let ctx = egui::Context::default();
        let long = "Paused. Viewers see the pause screen. Back in 4:32.";
        let (r, _) = draw(&ctx, 200.0, vec![], long);
        assert!(r.truncated);
        assert!(r.actions.iter().all(|a| a.rect.width() > 20.0 && r.rect.contains_rect(a.rect)));
    }

    #[test]
    fn a_banner_narrower_than_its_buttons_keeps_every_button_inside_and_clickable() {
        let ctx = egui::Context::default();
        let (r, _) = draw(&ctx, 90.0, vec![], "Paused.");
        for a in &r.actions {
            assert!(r.rect.contains_rect(a.rect) && a.rect.width() > 10.0, "{:?} in {:?}", a.rect, r.rect);
        }
        assert_eq!(r.glyph, None, "no glyph over the buttons");
        assert!(r.truncated, "no text over the buttons");
        assert!(r.actions[0].rect.right() <= r.actions[1].rect.left());
        let more = r.actions[0].rect.center();
        let (r, _) = draw(&ctx, 90.0, click(more), "Paused.");
        assert_eq!(r.clicked(), Some(0));
        let (wide, _) = draw(&ctx, 800.0, vec![], "Paused.");
        let Some(glyph) = wide.glyph else { panic!("a wide banner has its glyph") };
        assert!(wide.actions.iter().all(|a| !a.rect.intersects(glyph)));
    }

    #[test]
    fn a_right_to_left_banner_puts_its_buttons_at_the_left() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let seen: Rc<RefCell<Option<(Rect, Vec<Rect>)>>> = Rc::default();
        let mut f = {
            let (seen, st) = (Rc::clone(&seen), style());
            craft_ui::testing::Fixture::new(move |ui| {
                let r = Banner::new("Paused.", "Paused", &ACTIONS, &st).show(ui);
                *seen.borrow_mut() = Some((r.rect, r.actions.iter().map(|a| a.rect).collect()));
            })
        };
        f.room(vec2(800.0, 100.0)).direction(craft_ui::runtime::env::Direction::Rtl).frame(Vec::new());
        let Some((rect, actions)) = seen.take() else { panic!("the banner was not drawn") };
        let [more, resume] = [actions[0], actions[1]];
        assert!(resume.left() >= rect.left() && resume.left() < rect.left() + 20.0, "{resume:?} in {rect:?}");
        assert!(resume.right() < more.left(), "+1 min comes before Resume in reading order: {more:?} {resume:?}");
    }

    #[test]
    fn screen_readers_hear_the_announcement_and_the_button_names() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let (_, out) = draw(&ctx, 800.0, vec![], "Paused. Back in 4:32.");
        let Some(update) = out.platform_output.accesskit_update else { panic!("no accessibility tree") };
        let live: Vec<_> = update.nodes.iter().filter(|(_, n)| n.live() == Some(egui::accesskit::Live::Polite)).map(|(_, n)| n.label()).collect();
        assert_eq!(live, [Some("Paused")], "the announcement, not the visible text");
        for name in ["Add a Minute to the Pause", "Resume Program"] {
            assert!(update.nodes.iter().any(|(_, n)| n.role() == Role::Button && n.label() == Some(name)), "{name}");
        }
    }
}
