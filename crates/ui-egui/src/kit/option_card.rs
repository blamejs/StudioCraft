//! Option cards: two or more cards, each with a title and a description, exactly one of them
//! selected, as a set of radio buttons.
//!
//! The cards stand side by side, sharing the width equally and as tall as the tallest, when each
//! gets at least the style's minimum width, and one above the other otherwise. Each card has a
//! 1-point outline and a radio circle at its start; the selected card has the selection fill, an
//! accent border and a filled circle. Each card is its own focusable widget that screen readers
//! announce as a radio button named by its title, with its description as the description. The
//! focused card has the theme's focus ring. A click, Space or Enter selects a card, and the arrow
//! keys select the neighbor of the focused one. In a right-to-left interface the first card is at
//! the right, the circle is at each card's right and the text is aligned to the right.

use craft_ui::style::StyleCx;
use egui::text::LayoutJob;
use egui::{
    Align, Color32, CornerRadius, EventFilter, FontId, Galley, Id, Key, Modifiers, Rect, Response, Role, Sense, Stroke, StrokeKind, Ui, WidgetInfo,
    pos2, vec2,
};
use std::sync::Arc;

use super::corner;

/// The colors and sizes of [`OptionCards`].
#[derive(Clone, Debug, PartialEq)]
pub struct OptionCardStyle {
    pub fill: Color32,
    pub hover: Color32,
    pub outline: Color32,
    pub selected_fill: Color32,
    /// The border of the selected card and the dot in its circle.
    pub selected_border: Color32,
    /// The circle of a card that is not selected.
    pub ring: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    pub text_disabled: Color32,
    /// The space inside a card on each side.
    pub padding: f32,
    /// The space between cards, and between the circle and the text.
    pub gap: f32,
    /// The space between a card's title and its description.
    pub line_gap: f32,
    /// The circle's diameter.
    pub radio: f32,
    /// The narrowest a card is drawn beside the others; below it the cards stack.
    pub min_width: f32,
    pub radius: u8,
    pub title_font: FontId,
    pub body_font: FontId,
    /// The ring on the focused card.
    pub focus: Stroke,
}

impl OptionCardStyle {
    /// The style from the tokens of `cx`, with titles in `title_font`: the raised surface for a
    /// card, the selected fill with the accent text color as its border, the control edge for the
    /// outlines, the regular padding, the control radius and the caption role for descriptions.
    pub fn new(cx: &StyleCx<'_>, title_font: FontId) -> OptionCardStyle {
        let t = &cx.tokens;
        let look = cx.variant_look();
        let c = |rgba| cx.color(rgba).egui();
        OptionCardStyle {
            fill: c(t.color.surface.raised),
            hover: look.fill.off.hover.egui(),
            outline: c(t.color.line.control_edge),
            selected_fill: c(t.color.state.selected),
            selected_border: c(t.color.accent.text),
            ring: c(t.color.line.control_edge),
            text: c(t.color.text.normal),
            text_muted: c(t.color.text.muted),
            text_disabled: c(t.color.text.disabled),
            padding: t.space.pad.regular,
            gap: 8.0,
            line_gap: 4.0,
            radio: 14.0,
            min_width: 160.0,
            radius: corner(t.shape.radius.control),
            title_font,
            body_font: FontId::proportional(t.typography.caption.size),
            focus: cx.focus_look().stroke,
        }
    }
}

/// One card: its title and description, shown as given, already translated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OptionCard<'a> {
    pub title: &'a str,
    pub description: &'a str,
}

/// What [`OptionCards::show`] returns.
pub struct OptionCardsResponse {
    /// The whole set.
    pub rect: Rect,
    /// The card chosen this frame, when it differs from the selected one.
    pub changed: Option<usize>,
    /// One response per card, in order.
    pub cards: Vec<Response>,
    /// Whether the cards stand one above the other.
    pub stacked: bool,
}

/// A set of option cards across the available width.
pub struct OptionCards<'a> {
    id: Id,
    cards: &'a [OptionCard<'a>],
    selected: usize,
    style: &'a OptionCardStyle,
    enabled: bool,
}

/// The title and description of one card, laid out for its text width.
struct Laid {
    title: Arc<Galley>,
    body: Arc<Galley>,
}

impl Laid {
    /// The card's height around its text and circle.
    fn height(&self, s: &OptionCardStyle) -> f32 {
        let text = self.title.size().y + s.line_gap + self.body.size().y;
        text.max(s.radio) + 2.0 * s.padding
    }
}

impl<'a> OptionCards<'a> {
    /// `selected` past the last card selects none of them.
    pub fn new(id: Id, cards: &'a [OptionCard<'a>], selected: usize, style: &'a OptionCardStyle) -> OptionCards<'a> {
        OptionCards { id, cards, selected, style, enabled: true }
    }

    pub fn enabled(mut self, enabled: bool) -> OptionCards<'a> {
        self.enabled = enabled;
        self
    }

    /// Whether `width` holds every card side by side at the minimum width or more.
    pub fn fits_in_a_row(&self, width: f32) -> bool {
        let n = self.cards.len() as f32;
        width >= n * self.style.min_width + (n - 1.0).max(0.0) * self.style.gap
    }

    /// `text` in `font` and `color`, wrapped at `width` and aligned to the start of a line.
    fn layout(ui: &Ui, text: &str, font: &FontId, color: Color32, width: f32, rtl: bool) -> Arc<Galley> {
        let mut job = LayoutJob::simple(text.to_owned(), font.clone(), color, width);
        job.halign = if rtl { Align::RIGHT } else { Align::LEFT };
        ui.painter().layout_job(job)
    }

    pub fn show(self, ui: &mut Ui) -> OptionCardsResponse {
        let s = self.style;
        let rtl = craft_ui::runtime::env::Env::of(ui).view().direction().is_rtl();
        let n = self.cards.len();
        let available = ui.available_width();
        let fallback = n as f32 * s.min_width + n.saturating_sub(1) as f32 * s.gap;
        let width = if available.is_finite() && available > 0.0 { available } else { fallback };
        let stacked = !self.fits_in_a_row(width);
        let card_w = if stacked || n == 0 { width } else { (width - (n - 1) as f32 * s.gap) / n as f32 };
        let text_w = (card_w - 2.0 * s.padding - s.radio - s.gap).max(1.0);
        let title_color = if self.enabled { s.text } else { s.text_disabled };
        let laid: Vec<Laid> = self
            .cards
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let body_color = match (self.enabled, i == self.selected) {
                    (false, _) => s.text_disabled,
                    (true, true) => s.text,
                    (true, false) => s.text_muted,
                };
                Laid {
                    title: Self::layout(ui, c.title, &s.title_font, title_color, text_w, rtl),
                    body: Self::layout(ui, c.description, &s.body_font, body_color, text_w, rtl),
                }
            })
            .collect();
        let heights: Vec<f32> = laid.iter().map(|l| l.height(s)).collect();
        let row_h = heights.iter().copied().fold(0.0, f32::max);
        let total_h = if stacked { heights.iter().sum::<f32>() + n.saturating_sub(1) as f32 * s.gap } else { row_h };
        let (rect, _) = ui.allocate_exact_size(vec2(width, total_h), Sense::hover());
        // Each card's place: side by side from the start edge, or one under the other.
        let boxes: Vec<Rect> = (0..n)
            .scan(0.0_f32, |along, i| {
                let h = heights.get(i).copied().unwrap_or(0.0);
                let b = if stacked {
                    Rect::from_min_size(pos2(rect.left(), rect.top() + *along), vec2(width, h))
                } else {
                    let x = if rtl { rect.right() - *along - card_w } else { rect.left() + *along };
                    Rect::from_min_size(pos2(x, rect.top()), vec2(card_w, row_h))
                };
                let step = if stacked { h } else { card_w };
                *along += step + s.gap;
                Some(b)
            })
            .collect();
        // A measuring run of the framework draws the cards to size them and delivers no choice or
        // key, so the real run gets them.
        let live = !craft_ui::runtime::measure::active(ui.ctx());
        let painter = ui.painter_at(rect.expand(s.focus.width.max(1.0)));
        let radius = CornerRadius::same(s.radius);
        let mut cards = Vec::with_capacity(n);
        let mut changed = None;
        let last = n.saturating_sub(1);
        for (i, ((card, l), b)) in self.cards.iter().zip(&laid).zip(&boxes).enumerate() {
            let sense = if self.enabled { Sense::click() } else { Sense::hover() };
            let r = ui.interact(*b, self.id.with(i), sense);
            let selected = i == self.selected;
            r.widget_info(|| WidgetInfo::selected(Role::RadioButton, self.enabled, selected, card.title));
            let description = card.description.to_owned();
            ui.ctx().accesskit_node_builder(r.id, |node| node.set_description(description));
            let (fill, edge) = if selected {
                (s.selected_fill, s.selected_border)
            } else if self.enabled && (r.hovered() || r.is_pointer_button_down_on()) {
                (s.hover, s.outline)
            } else {
                (s.fill, s.outline)
            };
            painter.rect_filled(*b, radius, fill);
            painter.rect_stroke(*b, radius, Stroke::new(1.0, edge), StrokeKind::Inside);
            // The circle sits at the start edge, level with the title's first line.
            let first_line = l.title.rows.first().map_or(s.radio, |row| row.height());
            let cy = b.top() + s.padding + first_line.max(s.radio) / 2.0;
            let cx = if rtl { b.right() - s.padding - s.radio / 2.0 } else { b.left() + s.padding + s.radio / 2.0 };
            let circle_color = match (self.enabled, selected) {
                (false, _) => s.text_disabled,
                (true, true) => s.selected_border,
                (true, false) => s.ring,
            };
            painter.circle_stroke(pos2(cx, cy), s.radio / 2.0 - 0.5, Stroke::new(1.0, circle_color));
            if selected {
                painter.circle_filled(pos2(cx, cy), s.radio / 4.0, circle_color);
            }
            let title_top = b.top() + s.padding + (s.radio - first_line).max(0.0) / 2.0;
            let start = if rtl { b.right() - s.padding - s.radio - s.gap } else { b.left() + s.padding + s.radio + s.gap };
            // A galley aligned to the right extends to the left of its origin, so the origin is put
            // where the galley's own rectangle meets the text's edge.
            let at = |g: &Galley, top: f32| if rtl { pos2(start - g.rect.max.x, top) } else { pos2(start - g.rect.min.x, top) };
            painter.galley(at(&l.title, title_top), Arc::clone(&l.title), title_color);
            let body_top = title_top + l.title.size().y + s.line_gap;
            painter.galley(at(&l.body, body_top), Arc::clone(&l.body), title_color);
            if live && self.enabled && r.clicked() && !selected {
                changed = Some(i);
            }
            if live && self.enabled && r.has_focus() {
                // The focused card keeps the arrow keys, so egui does not move the focus by them
                // from the next frame on.
                let arrows = EventFilter { horizontal_arrows: true, vertical_arrows: true, ..EventFilter::default() };
                ui.memory_mut(|m| m.set_focus_lock_filter(r.id, arrows));
                // Up and the arrow toward the start edge go to the card before; Down and the arrow
                // toward the end edge go to the card after.
                let (back, ahead) = if rtl { (Key::ArrowRight, Key::ArrowLeft) } else { (Key::ArrowLeft, Key::ArrowRight) };
                let take = |k: Key| ui.input_mut(|inp| inp.consume_key(Modifiers::NONE, k));
                let to = if i > 0 && (take(back) || take(Key::ArrowUp)) {
                    Some(i - 1)
                } else if i < last && (take(ahead) || take(Key::ArrowDown)) {
                    Some(i + 1)
                } else {
                    None
                };
                if let Some(to) = to {
                    // The next card takes the arrow filter with the focus, so an arrow in the next
                    // frame does not also move the focus by egui's own navigation.
                    ui.memory_mut(|m| m.request_focus_with_filter(self.id.with(to), arrows));
                    if to != self.selected {
                        changed = Some(to);
                    }
                }
            }
            cards.push(r);
        }
        // The focus ring goes on the card that holds the focus after this frame's arrow keys.
        let focused = ui.memory(|m| m.focused());
        if let Some(b) = (0..n).find(|i| self.enabled && focused == Some(self.id.with(*i))).and_then(|i| boxes.get(i)) {
            painter.rect_stroke(*b, radius, s.focus, StrokeKind::Outside);
        }
        OptionCardsResponse { rect, changed, cards, stacked }
    }
}

#[cfg(test)]
mod tests {
    use egui::{Event, PointerButton, RawInput};

    use super::*;

    const CARDS: [OptionCard<'static>; 3] = [
        OptionCard { title: "First", description: "The first card's description." },
        OptionCard { title: "Second", description: "A longer description that wraps onto more than one line in a narrow card." },
        OptionCard { title: "Third", description: "The third." },
    ];

    fn style() -> OptionCardStyle {
        StyleCx::with_root(&crate::test_context(), |cx| OptionCardStyle::new(cx, FontId::proportional(13.0)))
    }

    fn raw(width: f32, events: Vec<Event>) -> RawInput {
        RawInput { events, screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(width, 600.0))), ..RawInput::default() }
    }

    fn run(ctx: &egui::Context, width: f32, events: Vec<Event>, selected: usize) -> (OptionCardsResponse, Vec<egui::epaint::ClippedShape>) {
        let st = style();
        let mut out = None;
        let mut full = ctx.run_ui(raw(width, events), |ui| {
            out = Some(OptionCards::new(Id::unique("cards"), &CARDS, selected, &st).show(ui));
        });
        full.textures_delta.clear();
        let Some(out) = out else { panic!("the cards were not drawn") };
        (out, full.shapes)
    }

    fn click(at: egui::Pos2) -> Vec<Event> {
        let b = |pressed| Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        vec![Event::PointerMoved(at), b(true), b(false)]
    }

    fn key(k: Key) -> Vec<Event> {
        vec![Event::Key { key: k, physical_key: Some(k), pressed: true, repeat: false, modifiers: Modifiers::NONE }]
    }

    #[test]
    fn wide_room_puts_the_cards_side_by_side_at_one_height() {
        let ctx = egui::Context::default();
        let (r, _) = run(&ctx, 800.0, vec![], 0);
        assert!(!r.stacked);
        let rects: Vec<Rect> = r.cards.iter().map(|c| c.rect).collect();
        assert!(rects[0].right() < rects[1].left() && rects[1].right() < rects[2].left(), "{rects:?}");
        assert!(rects.iter().all(|c| c.height() == rects[0].height() && c.top() == r.rect.top()), "{rects:?}");
        assert!((rects[0].width() - rects[2].width()).abs() < 0.01, "the cards share the width: {rects:?}");
        assert!(rects.iter().all(|c| r.rect.contains_rect(*c)));
    }

    #[test]
    fn narrow_room_stacks_the_cards_at_the_full_width() {
        let ctx = egui::Context::default();
        let (r, _) = run(&ctx, 300.0, vec![], 0);
        assert!(r.stacked);
        let rects: Vec<Rect> = r.cards.iter().map(|c| c.rect).collect();
        assert!(rects[0].bottom() < rects[1].top() && rects[1].bottom() < rects[2].top(), "{rects:?}");
        assert!(rects.iter().all(|c| c.width() == r.rect.width()), "{rects:?}");
        assert!(rects[1].height() > rects[2].height(), "a card is as tall as its own text: {rects:?}");
        assert!((r.rect.bottom() - rects[2].bottom()).abs() < 0.01);
    }

    #[test]
    fn the_selected_card_carries_the_selection_fill_and_border() {
        let ctx = egui::Context::default();
        let (r, shapes) = run(&ctx, 800.0, vec![], 1);
        let p = style();
        let filled =
            |i: usize| shapes.iter().any(|c| matches!(&c.shape, egui::Shape::Rect(s) if s.fill == p.selected_fill && s.rect == r.cards[i].rect));
        let bordered = |i: usize| {
            shapes
                .iter()
                .any(|c| matches!(&c.shape, egui::Shape::Rect(s) if s.stroke == Stroke::new(1.0, p.selected_border) && s.rect == r.cards[i].rect))
        };
        assert!(filled(1) && bordered(1), "the selected card has the fill and the border");
        assert!(!filled(0) && !bordered(0) && !filled(2), "the others have neither");
        let dots = shapes.iter().filter(|c| matches!(&c.shape, egui::Shape::Circle(s) if s.fill == p.selected_border)).count();
        assert_eq!(dots, 1, "only the selected card's circle is filled");
    }

    #[test]
    fn a_click_on_another_card_changes_the_choice_and_one_on_the_selected_does_not() {
        let ctx = egui::Context::default();
        let (r, _) = run(&ctx, 800.0, vec![], 0);
        let (first, third) = (r.cards[0].rect.center(), r.cards[2].rect.center());
        assert_eq!(run(&ctx, 800.0, click(third), 0).0.changed, Some(2));
        assert_eq!(run(&ctx, 800.0, vec![], 0).0.changed, None);
        assert_eq!(run(&ctx, 800.0, click(first), 0).0.changed, None, "the selected card stays");
    }

    #[test]
    fn arrow_keys_move_the_choice_from_the_focused_card() {
        let ctx = egui::Context::default();
        run(&ctx, 800.0, vec![], 0);
        ctx.memory_mut(|m| m.request_focus(Id::unique("cards").with(0usize)));
        // Each key is pressed after a frame with the focus where the last one left it, as between
        // two key presses.
        let press = |k: Key, selected: usize| {
            run(&ctx, 800.0, vec![], selected);
            run(&ctx, 800.0, vec![], selected);
            run(&ctx, 800.0, key(k), selected).0.changed
        };
        let focused = || ctx.memory(|m| m.focused());
        assert_eq!(press(Key::ArrowRight, 0), Some(1));
        assert_eq!(focused(), Some(Id::unique("cards").with(1usize)), "focus follows the choice");
        assert_eq!(press(Key::ArrowDown, 1), Some(2));
        assert_eq!(press(Key::ArrowDown, 2), None, "the last card has no next one");
        assert_eq!(focused(), Some(Id::unique("cards").with(2usize)), "the focus stays on the last card");
        assert_eq!(press(Key::ArrowUp, 2), Some(1));
        assert_eq!(press(Key::ArrowLeft, 1), Some(0));
        assert_eq!(press(Key::ArrowLeft, 0), None, "the first card has no card before it");
        assert_eq!(focused(), Some(Id::unique("cards").with(0usize)));
    }

    #[test]
    fn arrows_in_consecutive_frames_move_the_focus_one_card_each() {
        let ctx = egui::Context::default();
        let st = style();
        let below = Id::unique("below");
        // At 300 points the cards stack, with another focusable widget under the third one.
        let draw = |events: Vec<Event>, selected: usize| {
            let mut out = None;
            let mut full = ctx.run_ui(raw(300.0, events), |ui| {
                out = Some(OptionCards::new(Id::unique("held"), &CARDS, selected, &st).show(ui));
                let r = ui.interact(Rect::from_min_size(ui.cursor().min, vec2(100.0, 20.0)), below, Sense::click());
                ui.advance_cursor_after_rect(r.rect);
            });
            full.textures_delta.clear();
            out.unwrap_or_else(|| panic!("the cards were not drawn")).changed
        };
        draw(vec![], 0);
        ctx.memory_mut(|m| m.request_focus(Id::unique("held").with(0usize)));
        draw(vec![], 0);
        draw(vec![], 0);
        assert_eq!(draw(key(Key::ArrowDown), 0), Some(1));
        assert_eq!(draw(key(Key::ArrowDown), 1), Some(2));
        assert_eq!(ctx.memory(|m| m.focused()), Some(Id::unique("held").with(2usize)), "the focus stays on the third card");
    }

    #[test]
    fn the_focused_card_has_the_focus_ring() {
        let ctx = egui::Context::default();
        let p = style();
        let ringed = |shapes: &[egui::epaint::ClippedShape], rect: Rect| {
            shapes.iter().any(|c| matches!(&c.shape, egui::Shape::Rect(s) if s.stroke == p.focus && s.rect == rect))
        };
        let (r, shapes) = run(&ctx, 800.0, vec![], 0);
        assert!(!r.cards.iter().any(|c| ringed(&shapes, c.rect)), "no ring without focus");
        ctx.memory_mut(|m| m.request_focus(Id::unique("cards").with(2usize)));
        let (r, shapes) = run(&ctx, 800.0, vec![], 0);
        assert!(ringed(&shapes, r.cards[2].rect));
        assert!(!ringed(&shapes, r.cards[0].rect));
    }

    #[test]
    fn a_disabled_set_ignores_clicks_and_keys() {
        let ctx = egui::Context::default();
        let st = style();
        let draw = |events: Vec<Event>| {
            let mut out = None;
            let mut full =
                ctx.run_ui(raw(800.0, events), |ui| out = Some(OptionCards::new(Id::unique("off"), &CARDS, 0, &st).enabled(false).show(ui)));
            full.textures_delta.clear();
            out.unwrap_or_else(|| panic!("the cards were not drawn"))
        };
        let at = draw(vec![]).cards[1].rect.center();
        assert_eq!(draw(click(at)).changed, None);
        ctx.memory_mut(|m| m.request_focus(Id::unique("off").with(0usize)));
        assert_eq!(draw(key(Key::ArrowRight)).changed, None);
    }

    #[test]
    fn no_cards_take_no_room() {
        let ctx = egui::Context::default();
        let st = style();
        let mut out = None;
        let mut full = ctx.run_ui(raw(800.0, vec![]), |ui| out = Some(OptionCards::new(Id::unique("none"), &[], 0, &st).show(ui)));
        full.textures_delta.clear();
        let Some(r) = out else { panic!("the set was not drawn") };
        assert!(r.cards.is_empty() && r.changed.is_none());
        assert_eq!(r.rect.height(), 0.0);
    }

    #[test]
    fn a_right_to_left_set_starts_at_the_right_with_the_circle_at_each_cards_right() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let seen: Rc<RefCell<Option<Vec<Rect>>>> = Rc::default();
        let mut f = {
            let (seen, st) = (Rc::clone(&seen), style());
            craft_ui::testing::Fixture::new(move |ui| {
                let r = OptionCards::new(Id::unique("rtl"), &CARDS, 0, &st).show(ui);
                *seen.borrow_mut() = Some(r.cards.iter().map(|c| c.rect).collect());
            })
        };
        f.room(vec2(800.0, 300.0)).direction(craft_ui::runtime::env::Direction::Rtl).frame(Vec::new());
        let Some(rects) = seen.take() else { panic!("the cards were not drawn") };
        assert!(rects[0].left() > rects[1].right() && rects[1].left() > rects[2].right(), "the first card is at the right: {rects:?}");
        let title = f.texts().iter().find(|t| t.text == "First").map(|t| t.rect).unwrap_or_else(|| panic!("no title"));
        assert!(title.right() > rects[0].center().x, "the title ends at the card's right: {title:?} in {:?}", rects[0]);
        assert!(title.right() < rects[0].right() - 14.0, "the circle is right of the title: {title:?} in {:?}", rects[0]);
    }

    #[test]
    fn screen_readers_hear_each_card_as_a_radio_button_with_its_description() {
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let st = style();
        let mut full = ctx.run_ui(raw(800.0, vec![]), |ui| {
            let _ = OptionCards::new(Id::unique("a11y"), &CARDS, 1, &st).show(ui);
        });
        full.textures_delta.clear();
        let Some(update) = full.platform_output.accesskit_update else { panic!("no accessibility tree") };
        for card in &CARDS {
            let node = update.nodes.iter().find(|(_, n)| n.role() == Role::RadioButton && n.label() == Some(card.title));
            let Some((_, node)) = node else { panic!("no radio button named {}", card.title) };
            assert_eq!(node.description(), Some(card.description));
        }
        let on = Some(egui::accesskit::Toggled::True);
        let selected: Vec<_> = update.nodes.iter().filter(|(_, n)| n.role() == Role::RadioButton && n.toggled() == on).collect();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].1.label(), Some("Second"));
    }
}
