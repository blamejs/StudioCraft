//! Small icons drawn with shapes, on icon buttons.

use std::f32::consts::TAU;

use craft_ui::primitives::Glyph;
use craft_ui::style::UiTokens;
use egui::{Color32, CornerRadius, Painter, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Gear,
    /// Two arrows pointing apart: the transition picker when the output bar is narrow.
    Transition,
}

// port: P8 (icons, segmented control and on-air buttons) replaces this glyph with the framework's
// `volume` icon once the icon set has that meaning.

/// The speaker of an audio channel's mute button while the channel is heard: a speaker with two
/// sound waves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, craft_ui::contract::MemoProps)]
pub struct Speaker;

impl Glyph for Speaker {
    fn paint(&self, painter: &Painter, _: &UiTokens, rect: Rect, color: Color32, _: bool) {
        speaker(painter, rect, color);
    }
}

/// Draw the speaker with its two sound waves centered in `rect`.
fn speaker(p: &Painter, rect: Rect, color: Color32) {
    let c = rect.center();
    let s = rect.height().min(rect.width()) * 0.32;
    if !rect.is_finite() || s <= 0.0 {
        return;
    }
    let x0 = c.x - s;
    let pts = vec![
        pos2(x0, c.y - s * 0.35),
        pos2(x0 + s * 0.45, c.y - s * 0.35),
        pos2(x0 + s, c.y - s),
        pos2(x0 + s, c.y + s),
        pos2(x0 + s * 0.45, c.y + s * 0.35),
        pos2(x0, c.y + s * 0.35),
    ];
    p.add(Shape::convex_polygon(pts, color, Stroke::NONE));
    let w = pos2(x0 + s * 1.1, c.y);
    p.add(Shape::line(arc(w, s * 0.5, -0.9, 0.9, 8), Stroke::new(1.3, color)));
    p.add(Shape::line(arc(w, s * 0.95, -0.9, 0.9, 10), Stroke::new(1.3, color)));
}

// port: P8 (icons, segmented control and on-air buttons) replaces this button, the copy of the
// icon button craft-ui dropped, with the framework's `Icon` buttons.

/// An icon button's response, for callers that need its rectangle as well as the click. It takes
/// its colors from the current egui theme, with an inset outline while it has keyboard focus.
pub(crate) fn response(ui: &mut Ui, icon: Icon, tip: &str, enabled: bool, size: egui::Vec2, tint: Color32) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        let valid = size.is_finite() && size.x > 0.0 && size.y > 0.0 && size.x <= 4096.0 && size.y <= 4096.0;
        let (rect, response) =
            ui.allocate_exact_size(if valid { size } else { egui::Vec2::ZERO }, if valid { Sense::click() } else { Sense::hover() });
        response.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, valid && ui.is_enabled(), false, tip));
        if valid {
            let w = &ui.visuals().widgets;
            let state = if response.is_pointer_button_down_on() {
                &w.active
            } else if response.hovered() {
                &w.hovered
            } else {
                &w.inactive
            };
            let radius = CornerRadius::same(3);
            if state.weak_bg_fill != Color32::TRANSPARENT {
                ui.painter().rect_filled(rect, radius, state.weak_bg_fill);
            }
            let stroke = bounded_stroke(state.bg_stroke, rect);
            if !stroke.is_empty() {
                ui.painter().rect_stroke(rect, radius, stroke, StrokeKind::Inside);
            }
            let color = if tint == Color32::TRANSPARENT { state.fg_stroke.color } else { tint };
            let color = if ui.is_enabled() { color } else { color.gamma_multiply(0.4) };
            paint(ui, icon, rect, color);
            if response.has_focus() && rect.width() > 4.0 && rect.height() > 4.0 {
                let focus = rect.shrink(2.0);
                let stroke = bounded_stroke(Stroke::new(1.0, ui.visuals().selection.stroke.color), focus);
                if !stroke.is_empty() {
                    ui.painter().rect_stroke(focus, radius, stroke, StrokeKind::Inside);
                }
            }
        }
        response.on_hover_text(tip)
    })
    .inner
}

fn bounded_stroke(stroke: Stroke, rect: Rect) -> Stroke {
    if stroke.width.is_finite() && stroke.width > 0.0 {
        Stroke::new(stroke.width.min(rect.width().min(rect.height()) * 0.5), stroke.color)
    } else {
        Stroke::NONE
    }
}

/// A `size` icon button that opens a panel below it. Only a click outside the panel closes it, so
/// it can hold fields and sliders.
pub fn panel(ui: &mut Ui, icon: Icon, tip: &str, size: egui::Vec2, add: impl FnOnce(&mut Ui)) -> egui::Response {
    let r = response(ui, icon, tip, true, size, Color32::TRANSPARENT);
    egui::Popup::menu(&r).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).show(add);
    r
}

fn arc(c: Pos2, r: f32, from: f32, to: f32, n: usize) -> Vec<Pos2> {
    (0..=n).map(|i| from + (to - from) * i as f32 / n as f32).map(|a| pos2(c.x + r * a.cos(), c.y + r * a.sin())).collect()
}

/// Draw `icon` centered in `rect`.
pub fn paint(ui: &Ui, icon: Icon, rect: Rect, color: Color32) {
    let p = ui.painter();
    let c = rect.center();
    let s = rect.height().min(rect.width()) * 0.32;
    let line = Stroke::new(1.6, color);
    match icon {
        Icon::Gear => {
            for k in 0..8 {
                let a = k as f32 * TAU / 8.0;
                let dir = vec2(a.cos(), a.sin());
                p.line_segment([c + dir * s * 0.65, c + dir * s * 1.1], Stroke::new(2.2, color));
            }
            p.circle_stroke(c, s * 0.7, Stroke::new(1.8, color));
            p.circle_filled(c, s * 0.25, color);
        }
        Icon::Transition => {
            // An arrow to the right on top and one to the left below it.
            for (y, d) in [(-s * 0.45, 1.0), (s * 0.45, -1.0)] {
                let tip = c + vec2(d * s, y);
                p.line_segment([c + vec2(-d * s, y), tip], line);
                p.line_segment([tip, tip + vec2(-d * s * 0.5, -s * 0.4)], line);
                p.line_segment([tip, tip + vec2(-d * s * 0.5, s * 0.4)], line);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_paints() {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            for icon in [Icon::Gear, Icon::Transition] {
                assert!(!response(ui, icon, "icon", true, vec2(24.0, 24.0), Color32::TRANSPARENT).clicked());
                assert!(!response(ui, icon, "icon", false, vec2(24.0, 24.0), Color32::TRANSPARENT).clicked());
            }
        });
        out.textures_delta.clear();
        assert!(!out.shapes.is_empty());
    }

    #[test]
    fn the_speaker_glyph_paints_inside_its_square_and_not_in_an_empty_one() {
        let ctx = crate::test_context();
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            let tokens = craft_ui::style::StyleCx::with_root(ui.ctx(), |cx| cx.tokens.clone());
            let square = Rect::from_min_size(pos2(10.0, 10.0), vec2(16.0, 16.0));
            Speaker.paint(ui.painter(), &tokens, square, Color32::WHITE, false);
            Speaker.paint(ui.painter(), &tokens, Rect::from_min_size(pos2(40.0, 40.0), egui::Vec2::ZERO), Color32::WHITE, false);
            Speaker.paint(ui.painter(), &tokens, Rect::from_min_size(pos2(40.0, 40.0), vec2(f32::NAN, 16.0)), Color32::WHITE, false);
        });
        out.textures_delta.clear();
        assert_eq!(out.shapes.len(), 3, "the speaker and its two waves, and nothing for the empty squares");
        let square = Rect::from_min_size(pos2(10.0, 10.0), vec2(16.0, 16.0));
        for c in &out.shapes {
            let bounds = c.shape.visual_bounding_rect();
            assert!(square.expand(1.0).contains_rect(bounds), "{bounds:?} is inside {square:?}");
        }
    }
}
