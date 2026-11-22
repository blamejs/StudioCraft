//! A segmented control: two to four joined segments in one track, exactly one of them selected.
//!
//! The track has a 1-point outline; the selected segment has the selection fill, a 1-point
//! border and the full text color, and the others use the muted text color. Each segment is its
//! own focusable widget that screen readers announce as a radio button, and the focused one has
//! the theme's focus ring. A click, Space or Enter selects a segment, and the left and right arrow
//! keys select the neighbor of the focused one.

use craft_ui::style::StyleCx;
use egui::{Color32, CornerRadius, FontId, Id, Key, Modifiers, Rect, Response, Role, Sense, Stroke, StrokeKind, Ui, WidgetInfo, pos2, vec2};

use super::corner;

/// The colors and sizes of a [`Segmented`] control.
#[derive(Clone, Debug, PartialEq)]
pub struct SegmentedStyle {
    pub track: Color32,
    pub hover: Color32,
    pub outline: Color32,
    pub selected_fill: Color32,
    pub selected_border: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    pub text_disabled: Color32,
    /// The control's height.
    pub height: f32,
    /// The space on each side of a segment's label.
    pub padding: f32,
    pub radius: u8,
    /// The labels' font.
    pub font: FontId,
    /// The ring on the focused segment.
    pub focus: Stroke,
}

impl SegmentedStyle {
    /// The style from the tokens of `cx`: the secondary control look for the track, the selected
    /// fill with the accent text color as its border, the standard control height and the label
    /// role.
    pub fn new(cx: &StyleCx<'_>) -> SegmentedStyle {
        let t = &cx.tokens;
        let look = cx.variant_look();
        let c = |rgba| cx.color(rgba).egui();
        SegmentedStyle {
            track: look.fill.off.rest.egui(),
            hover: look.fill.off.hover.egui(),
            outline: look.edge.map_or_else(|| c(t.color.line.control_edge), |edge| edge.off.rest.egui()),
            selected_fill: c(t.color.state.selected),
            selected_border: c(t.color.accent.text),
            text: c(t.color.text.on_selected),
            text_muted: c(t.color.text.muted),
            text_disabled: c(t.color.text.disabled),
            height: t.size.control.md,
            padding: 8.0,
            radius: corner(t.shape.radius.control),
            font: FontId::proportional(t.typography.label.size),
            focus: cx.focus_look().stroke,
        }
    }
}

/// What [`Segmented::show`] returns.
pub struct SegmentedResponse {
    /// The whole control.
    pub rect: Rect,
    /// The segment chosen this frame, when it differs from the selected one.
    pub changed: Option<usize>,
    /// One response per segment, in order.
    pub segments: Vec<Response>,
}

/// A row of segments; `labels` are shown as given, already translated.
pub struct Segmented<'a> {
    id: Id,
    labels: &'a [&'a str],
    selected: usize,
    style: &'a SegmentedStyle,
    enabled: bool,
}

impl<'a> Segmented<'a> {
    /// `selected` past the last label selects none of them.
    pub fn new(id: Id, labels: &'a [&'a str], selected: usize, style: &'a SegmentedStyle) -> Segmented<'a> {
        Segmented { id, labels, selected, style, enabled: true }
    }

    pub fn enabled(mut self, enabled: bool) -> Segmented<'a> {
        self.enabled = enabled;
        self
    }

    /// Each segment's width: its label and the padding on both sides.
    pub fn segment_widths(&self, ui: &Ui) -> Vec<f32> {
        let font = self.style.font.clone();
        self.labels
            .iter()
            .map(|l| ui.painter().layout_no_wrap((*l).to_owned(), font.clone(), Color32::WHITE).size().x.ceil() + 2.0 * self.style.padding)
            .collect()
    }

    /// The width the control takes.
    pub fn width(&self, ui: &Ui) -> f32 {
        self.segment_widths(ui).iter().sum()
    }

    pub fn show(self, ui: &mut Ui) -> SegmentedResponse {
        let s = self.style;
        let widths = self.segment_widths(ui);
        let total: f32 = widths.iter().sum();
        let (rect, _) = ui.allocate_exact_size(vec2(total, s.height), Sense::hover());
        let radius = CornerRadius::same(s.radius);
        let painter = ui.painter_at(rect.expand(1.0));
        painter.rect_filled(rect, radius, s.track);
        let font = s.font.clone();
        let mut segments = Vec::with_capacity(widths.len());
        let mut changed = None;
        let mut shapes = Vec::with_capacity(widths.len());
        let mut x = rect.left();
        let last = widths.len().saturating_sub(1);
        for (i, (label, w)) in self.labels.iter().zip(&widths).enumerate() {
            let seg = Rect::from_min_max(pos2(x, rect.top()), pos2(x + w, rect.bottom()));
            x += w;
            let sense = if self.enabled { Sense::click() } else { Sense::hover() };
            let r = ui.interact(seg, self.id.with(i), sense);
            let selected = i == self.selected;
            r.widget_info(|| WidgetInfo::selected(Role::RadioButton, self.enabled, selected, *label));
            // The outer corners of the end segments follow the track's.
            let corners = CornerRadius {
                nw: if i == 0 { s.radius } else { 0 },
                sw: if i == 0 { s.radius } else { 0 },
                ne: if i == last { s.radius } else { 0 },
                se: if i == last { s.radius } else { 0 },
            };
            if selected {
                painter.rect_filled(seg, corners, s.selected_fill);
                painter.rect_stroke(seg, corners, Stroke::new(1.0, s.selected_border), StrokeKind::Inside);
            } else if self.enabled && (r.hovered() || r.is_pointer_button_down_on()) {
                painter.rect_filled(seg.shrink(1.0), corners, s.hover);
            }
            let color = match (self.enabled, selected) {
                (false, _) => s.text_disabled,
                (true, true) => s.text,
                (true, false) => s.text_muted,
            };
            painter.text(seg.center(), egui::Align2::CENTER_CENTER, *label, font.clone(), color);
            if self.enabled && r.clicked() && !selected {
                changed = Some(i);
            }
            shapes.push((seg, corners));
            if self.enabled && r.has_focus() {
                let left = i > 0 && ui.input_mut(|inp| inp.consume_key(Modifiers::NONE, Key::ArrowLeft));
                let right = i < last && ui.input_mut(|inp| inp.consume_key(Modifiers::NONE, Key::ArrowRight));
                let to = if left {
                    Some(i - 1)
                } else if right {
                    Some(i + 1)
                } else {
                    None
                };
                if let Some(to) = to {
                    ui.memory_mut(|m| m.request_focus(self.id.with(to)));
                    if to != self.selected {
                        changed = Some(to);
                    }
                }
            }
            segments.push(r);
        }
        painter.rect_stroke(rect, radius, Stroke::new(1.0, s.outline), StrokeKind::Inside);
        // The focus ring goes on the edge of the segment that holds the focus after this frame's
        // arrow keys, over the track's outline.
        let focused = ui.memory(|m| m.focused());
        let ring = (0..shapes.len()).find(|i| self.enabled && focused == Some(self.id.with(*i))).and_then(|i| shapes.get(i).copied());
        if let Some((seg, corners)) = ring {
            painter.rect_stroke(seg, corners, s.focus, StrokeKind::Inside);
        }
        SegmentedResponse { rect, changed, segments }
    }
}

#[cfg(test)]
mod tests {
    use egui::{Event, PointerButton, RawInput};

    use super::*;

    fn style() -> SegmentedStyle {
        StyleCx::with_root(&crate::test_context(), SegmentedStyle::new)
    }

    fn run(ctx: &egui::Context, events: Vec<Event>, selected: usize) -> (SegmentedResponse, Vec<egui::epaint::ClippedShape>) {
        let raw = RawInput { events, screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 100.0))), ..RawInput::default() };
        let st = style();
        let mut out = None;
        let mut full = ctx.run_ui(raw, |ui| {
            out = Some(Segmented::new(Id::unique("mode"), &["Edit", "Studio"], selected, &st).show(ui));
        });
        full.textures_delta.clear();
        let Some(out) = out else { panic!("the control was not drawn") };
        (out, full.shapes)
    }

    fn click(at: egui::Pos2) -> Vec<Event> {
        let b = |pressed| Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        vec![Event::PointerMoved(at), b(true), b(false)]
    }

    #[test]
    fn segments_are_as_tall_as_the_style_and_sized_to_their_labels() {
        let ctx = egui::Context::default();
        let (r, _) = run(&ctx, vec![], 0);
        assert_eq!(r.rect.height(), 24.0);
        assert_eq!(r.segments.len(), 2);
        let widths: Vec<f32> = r.segments.iter().map(|s| s.rect.width()).collect();
        assert!(widths[1] > widths[0], "Studio is longer than Edit: {widths:?}");
        for w in &widths {
            assert!(*w > 16.0 && *w < 80.0, "each segment is its label and 8 points on each side: {widths:?}");
        }
        assert_eq!(r.rect.width(), widths.iter().sum::<f32>());
        assert_eq!(r.segments[0].rect.right(), r.segments[1].rect.left(), "the segments are joined");
    }

    #[test]
    fn the_selected_segment_carries_the_selection_fill_and_border() {
        let ctx = egui::Context::default();
        let (r, shapes) = run(&ctx, vec![], 1);
        let p = style();
        let filled = shapes.iter().any(|c| matches!(&c.shape, egui::Shape::Rect(s) if s.fill == p.selected_fill && s.rect == r.segments[1].rect));
        assert!(filled, "no selection fill on the selected segment");
        let border = shapes
            .iter()
            .any(|c| matches!(&c.shape, egui::Shape::Rect(s) if s.stroke == Stroke::new(1.0, p.selected_border) && s.rect == r.segments[1].rect));
        assert!(border, "no border on the selected segment");
        let other = shapes.iter().any(|c| matches!(&c.shape, egui::Shape::Rect(s) if s.fill == p.selected_fill && s.rect == r.segments[0].rect));
        assert!(!other, "the other segment is not filled");
    }

    #[test]
    fn a_click_on_the_other_segment_changes_the_choice_and_one_on_the_selected_does_not() {
        let ctx = egui::Context::default();
        let (r, _) = run(&ctx, vec![], 0);
        let (edit, studio) = (r.segments[0].rect.center(), r.segments[1].rect.center());
        assert_eq!(run(&ctx, click(studio), 0).0.changed, Some(1));
        assert_eq!(run(&ctx, vec![], 0).0.changed, None);
        assert_eq!(run(&ctx, click(edit), 0).0.changed, None, "the selected segment stays");
    }

    #[test]
    fn arrow_keys_move_the_choice_from_the_focused_segment() {
        let ctx = egui::Context::default();
        run(&ctx, vec![], 0);
        ctx.memory_mut(|m| m.request_focus(Id::unique("mode").with(0usize)));
        run(&ctx, vec![], 0);
        let key = |k| vec![Event::Key { key: k, physical_key: Some(k), pressed: true, repeat: false, modifiers: Modifiers::NONE }];
        assert_eq!(run(&ctx, key(Key::ArrowRight), 0).0.changed, Some(1));
        assert_eq!(ctx.memory(|m| m.focused()), Some(Id::unique("mode").with(1usize)), "focus follows the choice");
        assert_eq!(run(&ctx, key(Key::ArrowRight), 1).0.changed, None, "the last segment has no right neighbor");
        assert_eq!(run(&ctx, key(Key::ArrowLeft), 1).0.changed, Some(0));
    }

    #[test]
    fn the_focused_segment_has_the_focus_ring() {
        let ctx = egui::Context::default();
        let p = style();
        let ringed = |shapes: &[egui::epaint::ClippedShape], rect: Rect| {
            shapes.iter().any(|c| matches!(&c.shape, egui::Shape::Rect(s) if s.stroke == p.focus && s.rect == rect))
        };
        let (r, shapes) = run(&ctx, vec![], 0);
        assert!(!r.segments.iter().any(|s| ringed(&shapes, s.rect)), "no ring without focus");
        ctx.memory_mut(|m| m.request_focus(Id::unique("mode").with(1usize)));
        let (r, shapes) = run(&ctx, vec![], 0);
        assert!(ringed(&shapes, r.segments[1].rect), "the focused segment has the ring");
        assert!(!ringed(&shapes, r.segments[0].rect));
        // ArrowLeft moves the focus to the first segment, and the ring moves with it in the same frame.
        let left =
            vec![Event::Key { key: Key::ArrowLeft, physical_key: Some(Key::ArrowLeft), pressed: true, repeat: false, modifiers: Modifiers::NONE }];
        let (r, shapes) = run(&ctx, left, 1);
        assert_eq!(r.changed, Some(0));
        assert!(ringed(&shapes, r.segments[0].rect), "the ring follows the focus");
        assert!(!ringed(&shapes, r.segments[1].rect));
    }

    #[test]
    fn a_disabled_control_ignores_clicks() {
        let ctx = egui::Context::default();
        let st = style();
        let mut at = egui::Pos2::ZERO;
        let raw = || RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 100.0))), ..RawInput::default() };
        let mut out =
            ctx.run_ui(raw(), |ui| at = Segmented::new(Id::unique("d"), &["A", "B"], 0, &st).enabled(false).show(ui).segments[1].rect.center());
        out.textures_delta.clear();
        let mut changed = Some(9);
        let mut out = ctx.run_ui(RawInput { events: click(at), ..raw() }, |ui| {
            changed = Segmented::new(Id::unique("d"), &["A", "B"], 0, &st).enabled(false).show(ui).changed;
        });
        out.textures_delta.clear();
        assert_eq!(changed, None);
    }
}
