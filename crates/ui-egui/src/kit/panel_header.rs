//! A dock's header: one row with the title on the left and the dock's actions on the right, or
//! the other way round in a right-to-left interface.
//!
//! The header is a fixed height and has a fill of its own and no separator under it. The actions
//! are laid out from the end edge, so they always keep their room; the title takes the width
//! that is left and is shortened with an ellipsis when it does not fit, with the whole title in its
//! tooltip. A dock has no footer: every action it offers is in its header.

use crate::i18n as direction;
use craft_ui::style::StyleCx;
use egui::{Color32, FontId, Rect, RichText, Sense, Ui, UiBuilder, pos2, vec2};

/// The colors and sizes of a [`panel_header`].
#[derive(Clone, Debug, PartialEq)]
pub struct PanelHeaderStyle {
    /// The header's height.
    pub height: f32,
    pub fill: Color32,
    pub title_color: Color32,
    pub title_font: FontId,
    /// The space between the start edge (the left, or the right in a right-to-left interface)
    /// and the title.
    pub title_inset: f32,
    /// The space between the last action and the end edge.
    pub action_inset: f32,
    /// The space between two actions, and between the title and the first action.
    pub action_gap: f32,
}

impl PanelHeaderStyle {
    /// The style from the tokens of `cx`, with the title in `title_font`.
    pub fn new(cx: &StyleCx<'_>, title_font: FontId) -> PanelHeaderStyle {
        let t = &cx.tokens;
        PanelHeaderStyle {
            height: t.size.panel_header,
            fill: cx.color(t.color.surface.raised).egui(),
            title_color: cx.color(t.color.text.muted).egui(),
            title_font,
            title_inset: 8.0,
            action_inset: 4.0,
            action_gap: 2.0,
        }
    }
}

/// What [`panel_header`] returns.
pub struct PanelHeaderResponse<R> {
    /// The whole header.
    pub rect: Rect,
    /// The room the title had.
    pub title: Rect,
    /// The room the actions took, at the right edge; zero wide when there are none.
    pub actions: Rect,
    /// The title did not fit and was shortened.
    pub truncated: bool,
    /// What `actions` returned.
    pub inner: R,
}

/// Draw a header across the available width with `title` and the actions `actions` adds. The
/// actions are added from the reading end, so the first one added is the rightmost (the leftmost
/// in a right-to-left interface).
pub fn panel_header<R>(ui: &mut Ui, title: &str, style: &PanelHeaderStyle, actions: impl FnOnce(&mut Ui) -> R) -> PanelHeaderResponse<R> {
    let width = ui.available_width().max(0.0);
    let height = if style.height.is_finite() { style.height.max(0.0) } else { 0.0 };
    let (rect, _) = ui.allocate_exact_size(vec2(width, height), Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter().rect_filled(rect, 0.0, style.fill);
    }
    let rtl = direction::is_rtl(ui);
    let (left_inset, right_inset) = if rtl { (style.action_inset, style.title_inset) } else { (style.title_inset, style.action_inset) };
    let inner_left = (rect.left() + left_inset).min(rect.right());
    let room = Rect::from_min_max(pos2(inner_left, rect.top()), pos2((rect.right() - right_inset).max(inner_left), rect.bottom()));
    let mut row = ui.new_child(UiBuilder::new().id_salt("panel-header-actions").max_rect(room).layout(direction::trailing(ui)));
    row.spacing_mut().item_spacing.x = style.action_gap;
    let inner = actions(&mut row);
    let used = row.min_rect();
    let end = direction::end_x(ui, room);
    let actions_rect = if used.width() > 0.0 { used.intersect(room) } else { Rect::from_min_max(pos2(end, room.top()), pos2(end, room.bottom())) };
    let gap = if actions_rect.width() > 0.0 { style.action_gap } else { 0.0 };
    let title_rect = if rtl {
        Rect::from_min_max(pos2((actions_rect.right() + gap).min(room.right()), room.top()), room.right_bottom())
    } else {
        Rect::from_min_max(room.left_top(), pos2((actions_rect.left() - gap).max(room.left()), room.bottom()))
    };
    let wanted = ui.painter().layout_no_wrap(title.to_owned(), style.title_font.clone(), style.title_color).size().x;
    let truncated = wanted > title_rect.width() + 0.5;
    if title_rect.width() > 0.0 {
        let mut col = ui.new_child(UiBuilder::new().id_salt("panel-header-title").max_rect(title_rect).layout(direction::row(ui)));
        col.set_clip_rect(title_rect.intersect(ui.clip_rect()));
        let text = RichText::new(title).font(style.title_font.clone()).color(style.title_color);
        let label = col.add(egui::Label::new(text).truncate().selectable(false));
        if truncated {
            label.on_hover_text(title);
        }
    }
    PanelHeaderResponse { rect, title: title_rect, actions: actions_rect, truncated, inner }
}

#[cfg(test)]
mod tests {
    use egui::{RawInput, Vec2};

    use super::*;

    fn style(ctx: &egui::Context) -> PanelHeaderStyle {
        StyleCx::with_root(ctx, |cx| PanelHeaderStyle::new(cx, FontId::proportional(12.0)))
    }

    /// One header `width` wide with `n` 24 x 24 actions; the header and the action rectangles.
    fn run(width: f32, title: &str, n: usize) -> (PanelHeaderResponse<Vec<Rect>>, Vec<egui::epaint::ClippedShape>) {
        run_in(false, width, title, n)
    }

    /// As [`run`], with the window's root direction right to left when `rtl` is set.
    fn run_in(rtl: bool, width: f32, title: &str, n: usize) -> (PanelHeaderResponse<Vec<Rect>>, Vec<egui::epaint::ClippedShape>) {
        use craft_ui::runtime::env::{Direction, RootEnv};
        use craft_ui::runtime::frame::FrameInput;

        let ctx = crate::test_context();
        let direction = if rtl { Direction::Rtl } else { Direction::Ltr };
        craft_ui::shell::embed::before_input(&ctx, |_| FrameInput::new(RootEnv::new().direction(direction)));
        let raw = RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(width, 200.0))), ..RawInput::default() };
        let st = style(&ctx);
        let mut out = None;
        let mut full = ctx.run_ui(raw, |ui| {
            out = Some(panel_header(ui, title, &st, |ui| {
                (0..n).map(|i| ui.add_sized(Vec2::splat(24.0), egui::Button::new(format!("{i}"))).rect).collect::<Vec<_>>()
            }));
        });
        full.textures_delta.clear();
        let Some(out) = out else { panic!("the header was not drawn") };
        (out, full.shapes)
    }

    #[test]
    fn the_header_takes_the_panel_header_height_with_actions_right_aligned_two_points_apart() {
        let (r, _) = run(300.0, "Scenes", 4);
        assert_eq!(r.rect.height(), style(&crate::test_context()).height);
        assert_eq!(r.rect.height(), 28.0, "the Compact panel header");
        let a = &r.inner;
        assert_eq!(a.len(), 4);
        assert!((r.rect.right() - a[0].right() - 4.0).abs() < 0.01, "the first action is 4 points from the right edge: {a:?}");
        for pair in a.windows(2) {
            assert!((pair[0].left() - pair[1].right() - 2.0).abs() < 0.01, "actions are 2 points apart: {a:?}");
        }
        for b in a {
            assert_eq!(b.height(), 24.0);
            assert!(b.top() >= r.rect.top() && b.bottom() <= r.rect.bottom(), "{b:?} is inside the header");
        }
        assert_eq!(r.title.left(), r.rect.left() + 8.0, "the title starts 8 points from the left");
        assert!(r.title.right() <= a[3].left(), "the title ends before the actions");
        assert!(!r.truncated);
    }

    #[test]
    fn a_right_to_left_header_mirrors_the_title_and_the_actions() {
        let (r, _) = run_in(true, 300.0, "Scenes", 4);
        let a = &r.inner;
        assert!((a[0].left() - r.rect.left() - 4.0).abs() < 0.01, "the first action is 4 points from the left edge: {a:?}");
        for pair in a.windows(2) {
            assert!((pair[1].left() - pair[0].right() - 2.0).abs() < 0.01, "actions are 2 points apart: {a:?}");
        }
        assert_eq!(r.title.right(), r.rect.right() - 8.0, "the title starts 8 points from the right");
        assert!(r.title.left() >= a[3].right(), "the title ends before the actions");
        let (r, _) = run_in(true, 200.0, "Audio Mixer", 0);
        assert_eq!(r.actions.width(), 0.0);
        assert_eq!(r.title.left(), r.rect.left() + 4.0);
    }

    #[test]
    fn the_header_has_its_fill_and_no_separator() {
        let (r, shapes) = run(300.0, "Sources", 2);
        let fill = style(&crate::test_context()).fill;
        assert!(shapes.iter().any(|c| matches!(&c.shape, egui::Shape::Rect(s) if s.fill == fill && s.rect == r.rect)), "no header fill");
        let line = shapes.iter().any(|c| match &c.shape {
            egui::Shape::LineSegment { points, .. } => (points[0].y - r.rect.bottom()).abs() < 1.0,
            _ => false,
        });
        assert!(!line, "no separator under the header");
    }

    #[test]
    fn a_narrow_header_keeps_its_actions_and_shortens_the_title() {
        let (r, _) = run(140.0, "A very long dock title that cannot fit", 4);
        assert!(r.truncated);
        for b in &r.inner {
            assert!(b.left() >= r.rect.left() && b.right() <= r.rect.right(), "{b:?} is inside the 140 point header");
        }
        assert!(r.title.right() <= r.inner[3].left());
    }

    #[test]
    fn a_header_without_actions_gives_the_title_the_whole_row() {
        let (r, _) = run(200.0, "Audio Mixer", 0);
        assert_eq!(r.actions.width(), 0.0);
        assert_eq!(r.title.right(), r.rect.right() - 4.0);
        assert!(!r.truncated);
    }

    #[test]
    fn a_header_with_no_room_draws_without_panicking() {
        for w in [0.0, 1.0, 10.0] {
            let (r, _) = run(w, "Scenes", 3);
            assert!(r.title.width() >= 0.0 && r.actions.width() >= 0.0);
        }
        let mut st = style(&crate::test_context());
        st.height = f32::NAN;
        let ctx = egui::Context::default();
        let mut h = None;
        let mut out = ctx.run_ui(RawInput::default(), |ui| h = Some(panel_header(ui, "X", &st, |_| ()).rect.height()));
        out.textures_delta.clear();
        assert_eq!(h, Some(0.0), "a height that is not a number draws an empty header");
    }
}
