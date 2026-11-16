//! Small icons drawn with shapes, on craft-ui's icon buttons.

use std::f32::consts::{PI, TAU};

use craft_ui::buttons::{ButtonVisuals, IconButton, IconButtonStyle};
use egui::{Color32, CornerRadius, Pos2, Rect, Shape, Stroke, Ui, pos2, vec2};

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
    /// The window buttons of the title bar the app draws.
    Minimize,
    Maximize,
    Restore,
    Close,
}

/// Button colors from the current egui theme.
pub fn style(ui: &Ui) -> IconButtonStyle {
    let w = &ui.visuals().widgets;
    let v = |s: &egui::style::WidgetVisuals| ButtonVisuals { fill: s.weak_bg_fill, stroke: s.bg_stroke, icon: s.fg_stroke.color };
    IconButtonStyle {
        normal: v(&w.inactive),
        hovered: v(&w.hovered),
        pressed: Some(v(&w.active)),
        selected: ButtonVisuals { fill: ui.visuals().selection.bg_fill, stroke: Stroke::NONE, icon: ui.visuals().selection.stroke.color },
        corner_radius: CornerRadius::same(3),
        focus_stroke: Stroke::new(1.0, ui.visuals().selection.stroke.color),
    }
}

/// An icon button with its own size and, unless transparent, its own icon color.
pub fn sized(ui: &mut Ui, icon: Icon, tip: &str, enabled: bool, size: egui::Vec2, tint: Color32) -> bool {
    response(ui, icon, tip, enabled, size, tint).clicked()
}

/// An icon button's response, for callers that need its rectangle as well as the click.
pub(crate) fn response(ui: &mut Ui, icon: Icon, tip: &str, enabled: bool, size: egui::Vec2, tint: Color32) -> egui::Response {
    let st = style(ui);
    ui.add_enabled_ui(enabled, |ui| {
        IconButton::new(tip, size, &st)
            .show(ui, |ui, rect, color| {
                let color = if tint == Color32::TRANSPARENT { color } else { tint };
                let color = if ui.is_enabled() { color } else { color.gamma_multiply(0.4) };
                paint(ui, icon, rect, color);
            })
            .on_hover_text(tip)
    })
    .inner
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
        Icon::Minimize => {
            p.line_segment([c + vec2(-s, s * 0.6), c + vec2(s, s * 0.6)], Stroke::new(1.2, color));
        }
        Icon::Maximize => {
            p.rect_stroke(Rect::from_center_size(c, vec2(s * 2.0, s * 2.0)), 0.0, Stroke::new(1.2, color), egui::StrokeKind::Middle);
        }
        Icon::Restore => {
            let back = Rect::from_center_size(c + vec2(s * 0.3, -s * 0.3), vec2(s * 1.5, s * 1.5));
            let front = Rect::from_center_size(c + vec2(-s * 0.3, s * 0.3), vec2(s * 1.5, s * 1.5));
            let thin = Stroke::new(1.2, color);
            p.line_segment([pos2(back.left(), back.top()), pos2(back.right(), back.top())], thin);
            p.line_segment([pos2(back.right(), back.top()), pos2(back.right(), back.bottom())], thin);
            p.rect_stroke(front, 0.0, thin, egui::StrokeKind::Middle);
        }
        Icon::Close => {
            let thin = Stroke::new(1.2, color);
            p.line_segment([c + vec2(-s, -s), c + vec2(s, s)], thin);
            p.line_segment([c + vec2(-s, s), c + vec2(s, -s)], thin);
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
                Icon::Minimize,
                Icon::Maximize,
                Icon::Restore,
                Icon::Close,
            ] {
                assert!(!sized(ui, icon, "icon", true, vec2(24.0, 24.0), Color32::TRANSPARENT));
                assert!(!sized(ui, icon, "icon", false, vec2(24.0, 24.0), Color32::TRANSPARENT));
            }
        });
        out.textures_delta.clear();
        assert!(!out.shapes.is_empty());
    }
}
