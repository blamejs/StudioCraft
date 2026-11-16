//! A segmented control: two to four joined segments in one track, exactly one of them selected.
//!
//! The track has a 1-point outline; the selected segment has the selection fill, a 1-point
//! border and the full text color, and the others use the muted text color. Each segment is its
//! own focusable widget that screen readers announce as a radio button. A click, Space or Enter
//! selects a segment, and the left and right arrow keys select the neighbor of the focused one.

use egui::{
    Color32, CornerRadius, FontId, Id, Key, Modifiers, Rect, Response, Role, Sense, Stroke, StrokeKind, TextStyle, Ui, WidgetInfo, pos2, vec2,
};

use super::tokens::Palette;

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
}

impl SegmentedStyle {
    /// The style from a palette, with the control `height` tall and corners of `radius`.
    pub fn new(p: &Palette, height: f32, radius: u8) -> SegmentedStyle {
        SegmentedStyle {
            track: p.control,
            hover: p.control_hover,
            outline: p.line_strong,
            selected_fill: p.selection,
            selected_border: p.accent_text,
            text: p.text,
            text_muted: p.text_muted,
            text_disabled: p.text_disabled,
            height,
            padding: 8.0,
            radius,
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

    fn font(ui: &Ui) -> FontId {
        TextStyle::Button.resolve(ui.style())
    }

    /// Each segment's width: its label and the padding on both sides.
    pub fn segment_widths(&self, ui: &Ui) -> Vec<f32> {
        let font = Self::font(ui);
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
        let font = Self::font(ui);
        let mut segments = Vec::with_capacity(widths.len());
        let mut changed = None;
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
        SegmentedResponse { rect, changed, segments }
    }
}

#[cfg(test)]
mod tests {
    use egui::{Event, PointerButton, RawInput};

    use super::*;

    fn style() -> SegmentedStyle {
        SegmentedStyle::new(&Palette::DARK, 24.0, 4)
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
        let p = Palette::DARK;
        let filled = shapes.iter().any(|c| matches!(&c.shape, egui::Shape::Rect(s) if s.fill == p.selection && s.rect == r.segments[1].rect));
        assert!(filled, "no selection fill on the selected segment");
        let border = shapes
            .iter()
            .any(|c| matches!(&c.shape, egui::Shape::Rect(s) if s.stroke == Stroke::new(1.0, p.accent_text) && s.rect == r.segments[1].rect));
        assert!(border, "no border on the selected segment");
        let other = shapes.iter().any(|c| matches!(&c.shape, egui::Shape::Rect(s) if s.fill == p.selection && s.rect == r.segments[0].rect));
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
