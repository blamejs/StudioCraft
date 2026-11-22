//! Small icons drawn with shapes, on icon buttons.

use std::f32::consts::{PI, TAU};

use egui::{Color32, CornerRadius, Pos2, Rect, Sense, Shape, Stroke, StrokeKind, Ui, pos2, vec2};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Plus,
    Minus,
    Up,
    Down,
    Gear,
    Lock,
    Unlock,
    Speaker,
    Muted,
    Eye,
    EyeOff,
    /// Two arrows pointing apart: the transition picker when the output bar is narrow.
    Transition,
}

/// An icon button with its own size and, unless transparent, its own icon color.
pub fn sized(ui: &mut Ui, icon: Icon, tip: &str, enabled: bool, size: egui::Vec2, tint: Color32) -> bool {
    response(ui, icon, tip, enabled, size, tint).clicked()
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

/// An icon button that opens a menu below it.
pub fn menu(ui: &mut Ui, icon: Icon, tip: &str, add: impl FnOnce(&mut Ui)) {
    menu_sized(ui, icon, tip, vec2(26.0, 22.0), true, add);
}

/// A `size` icon button that opens a menu below it; its response.
pub fn menu_sized(ui: &mut Ui, icon: Icon, tip: &str, size: egui::Vec2, enabled: bool, add: impl FnOnce(&mut Ui)) -> egui::Response {
    let r = response(ui, icon, tip, enabled, size, Color32::TRANSPARENT);
    egui::Popup::menu(&r).show(add);
    r
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
        Icon::Plus => {
            p.line_segment([c - vec2(s, 0.0), c + vec2(s, 0.0)], line);
            p.line_segment([c - vec2(0.0, s), c + vec2(0.0, s)], line);
        }
        Icon::Minus => {
            p.line_segment([c - vec2(s, 0.0), c + vec2(s, 0.0)], line);
        }
        Icon::Up | Icon::Down => {
            let d = if icon == Icon::Up { -1.0 } else { 1.0 };
            let pts = vec![c + vec2(0.0, d * s * 0.8), c + vec2(s, -d * s * 0.6), c + vec2(-s, -d * s * 0.6)];
            p.add(Shape::convex_polygon(pts, color, Stroke::NONE));
        }
        Icon::Gear => {
            for k in 0..8 {
                let a = k as f32 * TAU / 8.0;
                let dir = vec2(a.cos(), a.sin());
                p.line_segment([c + dir * s * 0.65, c + dir * s * 1.1], Stroke::new(2.2, color));
            }
            p.circle_stroke(c, s * 0.7, Stroke::new(1.8, color));
            p.circle_filled(c, s * 0.25, color);
        }
        Icon::Lock | Icon::Unlock => {
            let body = Rect::from_center_size(c + vec2(0.0, s * 0.35), vec2(s * 1.6, s * 1.1));
            p.rect_filled(body, 1.5, color);
            let shift = if icon == Icon::Unlock { s * 0.55 } else { 0.0 };
            let top = pos2(c.x + shift, body.top());
            let r = s * 0.5;
            let mut pts = vec![pos2(top.x - r, top.y)];
            pts.extend(arc(pos2(top.x, top.y - s * 0.25), r, PI, TAU, 10));
            pts.push(pos2(top.x + r, top.y - if icon == Icon::Unlock { s * 0.25 } else { 0.0 }));
            p.add(Shape::line(pts, line));
        }
        Icon::Speaker | Icon::Muted => {
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
            if icon == Icon::Speaker {
                let w = pos2(x0 + s * 1.1, c.y);
                p.add(Shape::line(arc(w, s * 0.5, -0.9, 0.9, 8), Stroke::new(1.3, color)));
                p.add(Shape::line(arc(w, s * 0.95, -0.9, 0.9, 10), Stroke::new(1.3, color)));
            } else {
                let a = pos2(c.x + s * 0.35, c.y - s * 0.45);
                let b = pos2(c.x + s * 1.15, c.y + s * 0.45);
                p.line_segment([a, b], line);
                p.line_segment([pos2(a.x, b.y), pos2(b.x, a.y)], line);
            }
        }
        Icon::Eye | Icon::EyeOff => {
            let r = s * 1.6;
            let mut pts = arc(pos2(c.x, c.y + r * 0.55), r, -PI * 0.78, -PI * 0.22, 12);
            pts.extend(arc(pos2(c.x, c.y - r * 0.55), r, PI * 0.22, PI * 0.78, 12));
            p.add(Shape::closed_line(pts, Stroke::new(1.3, color)));
            p.circle_filled(c, s * 0.38, color);
            if icon == Icon::EyeOff {
                p.line_segment([c + vec2(-s * 1.1, s * 0.9), c + vec2(s * 1.1, -s * 0.9)], Stroke::new(1.8, color));
            }
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
            for icon in [
                Icon::Plus,
                Icon::Minus,
                Icon::Up,
                Icon::Down,
                Icon::Gear,
                Icon::Lock,
                Icon::Unlock,
                Icon::Speaker,
                Icon::Muted,
                Icon::Eye,
                Icon::EyeOff,
                Icon::Transition,
            ] {
                assert!(!sized(ui, icon, "icon", true, vec2(24.0, 24.0), Color32::TRANSPARENT));
                assert!(!sized(ui, icon, "icon", false, vec2(24.0, 24.0), Color32::TRANSPARENT));
            }
        });
        out.textures_delta.clear();
        assert!(!out.shapes.is_empty());
    }
}
