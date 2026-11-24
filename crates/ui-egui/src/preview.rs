//! The preview and program views: the compositor's canvas textures drawn in the window, and the
//! handles that move and resize the selected item. Moves and resizes snap to the canvas's edges,
//! center lines and safe areas and to other items' edges and center lines; Alt (Option on macOS)
//! turns snapping off while it is held. A corner drag keeps the aspect ratio unless Shift is held,
//! and resizes around the item's center while Ctrl (Command on macOS) is held.

use egui::{Color32, Pos2, Rect, Sense, Stroke, StrokeKind, TextureId, Ui, Vec2 as EVec2, pos2, vec2};
use studiocraft_model::geom::{MAX_SCALE, MIN_SCALE};
use studiocraft_model::{ItemId, Transform, Vec2};
use studiocraft_render::{CanvasViews, Ring};

/// The canvas textures registered with egui's renderer.
#[derive(Default)]
pub struct Textures {
    generation: Option<u64>,
    program: Vec<TextureId>,
    preview: Vec<TextureId>,
}

impl Textures {
    /// Register the views of a new canvas size, and free the old ones.
    pub fn sync(&mut self, rs: &egui_wgpu::RenderState, views: &CanvasViews) {
        if self.generation == Some(views.generation) {
            return;
        }
        let mut r = rs.renderer.write();
        for id in self.program.drain(..).chain(self.preview.drain(..)) {
            r.free_texture(&id);
        }
        let mut reg = |ring: &Ring| -> Vec<TextureId> {
            ring.views().iter().map(|v| r.register_native_texture(&rs.device, v, egui_wgpu::wgpu::FilterMode::Linear)).collect()
        };
        self.program = reg(&views.program);
        self.preview = reg(&views.preview);
        self.generation = Some(views.generation);
    }

    pub fn program(&self, views: &CanvasViews) -> Option<TextureId> {
        views.program.latest().and_then(|i| self.program.get(i).copied())
    }

    pub fn preview(&self, views: &CanvasViews) -> Option<TextureId> {
        views.preview.latest().and_then(|i| self.preview.get(i).copied())
    }
}

/// Where the canvas is drawn in the window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub rect: Rect,
    /// Screen points per canvas pixel.
    pub scale: f32,
}

impl View {
    /// The largest rectangle of the canvas's shape centered in `area`.
    pub fn fit(area: Rect, canvas: (u32, u32)) -> View {
        let (w, h) = (canvas.0.max(1) as f32, canvas.1.max(1) as f32);
        let scale = (area.width() / w).min(area.height() / h).max(0.0001);
        let rect = Rect::from_center_size(area.center(), vec2(w * scale, h * scale));
        View { rect, scale }
    }

    pub fn to_screen(self, p: Vec2) -> Pos2 {
        self.rect.min + vec2(p.x, p.y) * self.scale
    }

    pub fn to_canvas(self, p: Pos2) -> Vec2 {
        let d = (p - self.rect.min) / self.scale;
        Vec2::new(d.x, d.y)
    }
}

/// Draw the canvas picture (black until the first frame is ready).
pub fn paint_canvas(ui: &Ui, view: View, texture: Option<TextureId>) {
    let painter = ui.painter_at(view.rect.expand(1.0));
    painter.rect_filled(view.rect, 0.0, Color32::BLACK);
    if let Some(t) = texture {
        painter.image(t, view.rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
    }
}

/// An item as the editor sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EditItem {
    pub id: ItemId,
    pub transform: Transform,
    /// The source's own size; `None` until it has shown something.
    pub native: Option<(u32, u32)>,
    pub locked: bool,
    pub visible: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Grip {
    Move,
    /// Clockwise from the top left.
    Corner(usize),
}

#[derive(Clone, Debug)]
struct Drag {
    item: ItemId,
    grip: Grip,
    start: Vec2,
    from: Transform,
    native: (u32, u32),
    /// What the dragged item's edges snap to, gathered when the drag starts.
    lines: SnapLines,
}

/// The lines a dragged item's edges and center snap to, in canvas pixels: the vertical lines'
/// `x` and the horizontal lines' `y`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SnapLines {
    pub x: Vec<f32>,
    pub y: Vec<f32>,
}

impl SnapLines {
    /// The canvas's edges and center lines, the edges and center lines of every other visible
    /// item that has shown something and is turned by a multiple of 90 degrees, and the edges of
    /// the canvas's safe areas ([`SAFE_MARGINS`]).
    pub fn new(canvas: (u32, u32), items: &[EditItem], dragged: ItemId) -> SnapLines {
        let (w, h) = (canvas.0 as f32, canvas.1 as f32);
        let mut lines = SnapLines { x: vec![0.0, w * 0.5, w], y: vec![0.0, h * 0.5, h] };
        for it in items.iter().filter(|it| it.visible && it.id != dragged) {
            let Some(n) = it.native else { continue };
            let turn = it.transform.rotation.rem_euclid(90.0);
            if turn > 0.01 && turn < 89.99 {
                continue;
            }
            let c = it.transform.corners(n);
            let (mut min, mut max) = (c[0], c[0]);
            for k in &c[1..] {
                min = Vec2::new(min.x.min(k.x), min.y.min(k.y));
                max = Vec2::new(max.x.max(k.x), max.y.max(k.y));
            }
            if !(min.is_finite() && max.is_finite()) {
                continue;
            }
            lines.x.extend([min.x, (min.x + max.x) * 0.5, max.x]);
            lines.y.extend([min.y, (min.y + max.y) * 0.5, max.y]);
        }
        for m in SAFE_MARGINS {
            lines.x.extend([w * m, w * (1.0 - m)]);
            lines.y.extend([h * m, h * (1.0 - m)]);
        }
        lines
    }
}

/// The edges of the canvas's safe areas, as a share of each side measured from its edges: the
/// action safe area and the title safe area.
pub const SAFE_MARGINS: [f32; 2] = [0.035, 0.05];

/// The lines a drag snapped to this frame, drawn as guides: a vertical line at `x`, a horizontal
/// line at `y`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Guides {
    pub x: Option<f32>,
    pub y: Option<f32>,
}

/// The line in `lines` nearest to one of `edges`, when it is at most `snap` away: the distance
/// to move by and the line.
fn nearest(edges: &[f32], lines: impl Iterator<Item = f32> + Clone, snap: f32) -> Option<(f32, f32)> {
    let mut best: Option<(f32, f32)> = None;
    for e in edges {
        for l in lines.clone() {
            let d = l - e;
            if d.abs() <= snap && best.is_none_or(|(b, _)| d.abs() < b.abs()) {
                best = Some((d, l));
            }
        }
    }
    best
}

/// What happened in the editable view this frame.
#[derive(Default, Debug, PartialEq)]
pub struct EditEvent {
    /// The selection changed (to nothing, or to an item).
    pub select: Option<Option<ItemId>>,
    /// A drag gave an item a new transform.
    pub transform: Option<(ItemId, Transform)>,
    /// A drag ended.
    pub released: bool,
    /// The view was right-clicked.
    pub context: bool,
}

/// Screen distance at which a moved or resized item snaps to a line.
const SNAP_POINTS: f32 = 8.0;
/// The color of the lines a drag snapped to.
const GUIDE: Color32 = Color32::from_rgb(255, 64, 160);
const HANDLE: f32 = 7.0;

#[derive(Default)]
pub struct Editor {
    drag: Option<Drag>,
}

/// How a corner drag resizes an item.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResizeMode {
    /// The aspect ratio may change (Shift held).
    pub free: bool,
    /// The item's center stays in place and the opposite edges move by the same amount (Ctrl held,
    /// Command on macOS). Otherwise the opposite corner stays in place.
    pub centered: bool,
}

/// The point that stays in place while corner `i` is dragged, and the direction from it to corner
/// `i`, in the item's own box: the opposite corner, or the center when `centered`.
fn anchor(i: usize, size: Vec2, centered: bool) -> (Vec2, (f32, f32)) {
    let (a, dir) = match i {
        0 => (Vec2::new(size.x, size.y), (-1.0, -1.0)),
        1 => (Vec2::new(0.0, size.y), (1.0, -1.0)),
        2 => (Vec2::new(0.0, 0.0), (1.0, 1.0)),
        _ => (Vec2::new(size.x, 0.0), (-1.0, 1.0)),
    };
    if centered { (Vec2::new(size.x * 0.5, size.y * 0.5), dir) } else { (a, dir) }
}

/// The share of the item's width and height between the fixed point and the dragged corner: all
/// of it from the opposite corner, half of it from the center.
fn reach(mode: ResizeMode) -> f32 {
    if mode.centered { 0.5 } else { 1.0 }
}

/// `from` resized by dragging corner `corner` to the canvas point `to`, keeping the opposite
/// corner in place, or the center when `mode.centered`, and the aspect ratio unless `mode.free`.
pub fn resize(from: Transform, native: (u32, u32), corner: usize, to: Vec2, mode: ResizeMode) -> Transform {
    let size = from.size(native);
    if size.x <= 0.0 || size.y <= 0.0 {
        return from;
    }
    let p = from.to_local(native, to);
    let (a, dir) = anchor(corner, size, mode.centered);
    let r = reach(mode);
    let mut new = Vec2::new(((p.x - a.x) * dir.0 / r).max(1.0), ((p.y - a.y) * dir.1 / r).max(1.0));
    if !mode.free {
        let k = (new.x / size.x).max(new.y / size.y);
        new = Vec2::new(size.x * k, size.y * k);
    }
    // The scale is clamped to its limits, so the box is placed by the size it really gets.
    let t = from.with_size(native, new);
    let new = t.size(native);
    let min = if mode.centered {
        Vec2::new(a.x - new.x * 0.5, a.y - new.y * 0.5)
    } else {
        Vec2::new(if dir.0 < 0.0 { a.x - new.x } else { a.x }, if dir.1 < 0.0 { a.y - new.y } else { a.y })
    };
    // The new box's center, from the item's own box to the canvas.
    let local = Vec2::new(min.x + new.x * 0.5 - size.x * 0.5, min.y + new.y * 0.5 - size.y * 0.5);
    let c0 = from.center(native);
    let (s, co) = from.rotation.to_radians().sin_cos();
    let c = Vec2::new(c0.x + local.x * co - local.y * s, c0.y + local.x * s + local.y * co);
    Transform { position: Vec2::new(c.x - new.x * 0.5, c.y - new.y * 0.5), ..t }.sanitized()
}

/// [`resize`], with the moving edges snapped to `lines` when an edge of an unrotated item comes
/// within `snap` canvas pixels of one. With the aspect ratio kept, the edge that has the shorter
/// way to its line snaps and the other edge follows the ratio; with `mode.free`, each edge snaps
/// on its own. The opposite corner stays in place. With `mode.centered` the center stays in place
/// and both edges on each axis move, so either of them can snap and the edge opposite it moves by
/// the same amount. A line the item can't reach within its scale limits is not snapped to.
pub fn resized(from: Transform, native: (u32, u32), corner: usize, to: Vec2, mode: ResizeMode, lines: &SnapLines, snap: f32) -> (Transform, Guides) {
    let r = resize(from, native, corner, to, mode);
    let size = from.size(native);
    if snap <= 0.0 || from.rotation != 0.0 || size.x <= 0.0 || size.y <= 0.0 {
        return (r, Guides::default());
    }
    let (a, dir) = anchor(corner, size, mode.centered);
    let k = reach(mode);
    let fixed = Vec2::new(from.position.x + a.x, from.position.y + a.y);
    let now = r.size(native);
    let edge = Vec2::new(fixed.x + dir.0 * now.x * k, fixed.y + dir.1 * now.y * k);
    // The width or height that puts an edge on the line.
    let across = |l: f32, at: f32| (l - at).abs() / k;
    // The widths and heights the scale limits allow: at least a pixel, and the item's own size
    // times the smallest and largest scale.
    let (cw, ch) = from.crop.apply(native.0, native.1);
    let range = |n: u32| ((n as f32 * MIN_SCALE).max(1.0), n as f32 * MAX_SCALE);
    let (rx, ry) = (range(cw), range(ch));
    // A line the item can't reach, or one on the far side of the fixed corner that would turn the
    // item over, is not snapped to. The limits allow for a thousandth of rounding, so a line on
    // an edge held at a limit still counts.
    let reachable = |l: f32, at: f32, d: f32, (lo, hi): (f32, f32)| {
        let w = across(l, at);
        (mode.centered || (l - at) * d > 0.0) && w >= lo * 0.999 && w <= hi * 1.001
    };
    let xs = lines.x.iter().copied().filter(|l| reachable(*l, fixed.x, dir.0, rx));
    let ys = lines.y.iter().copied().filter(|l| reachable(*l, fixed.y, dir.1, ry));
    let (sx, sy) = if mode.centered {
        // Both edges on each axis move, so either of them can snap.
        let back = Vec2::new(fixed.x - dir.0 * now.x * k, fixed.y - dir.1 * now.y * k);
        (nearest(&[edge.x, back.x], xs, snap), nearest(&[edge.y, back.y], ys, snap))
    } else {
        (nearest(&[edge.x], xs, snap), nearest(&[edge.y], ys, snap))
    };
    let corner_at = |w: f32, h: f32| Vec2::new(fixed.x + dir.0 * w * k, fixed.y + dir.1 * h * k);
    if mode.free {
        if sx.is_none() && sy.is_none() {
            return (r, Guides::default());
        }
        let w = sx.map_or(now.x, |(_, l)| across(l, fixed.x));
        let h = sy.map_or(now.y, |(_, l)| across(l, fixed.y));
        return (resize(from, native, corner, corner_at(w, h), mode), Guides { x: sx.map(|s| s.1), y: sy.map(|s| s.1) });
    }
    let (s, guides) = match (sx, sy) {
        (Some((dx, l)), Some((dy, _))) if dx.abs() <= dy.abs() => (across(l, fixed.x) / size.x, Guides { x: Some(l), y: None }),
        (_, Some((_, l))) => (across(l, fixed.y) / size.y, Guides { x: None, y: Some(l) }),
        (Some((_, l)), None) => (across(l, fixed.x) / size.x, Guides { x: Some(l), y: None }),
        (None, None) => return (r, Guides::default()),
    };
    (resize(from, native, corner, corner_at(size.x * s, size.y * s), mode), guides)
}

/// `from` moved by `delta`, snapped to `lines` when an edge or the center line of an unrotated
/// item comes within `snap` canvas pixels of one. Each axis snaps to its nearest line.
pub fn moved(from: Transform, native: (u32, u32), delta: Vec2, lines: &SnapLines, snap: f32) -> (Transform, Guides) {
    let mut t = from;
    t.position = Vec2::new(from.position.x + delta.x, from.position.y + delta.y);
    let mut guides = Guides::default();
    if snap > 0.0 && from.rotation == 0.0 {
        let (p, s) = (t.position, t.size(native));
        if let Some((d, l)) = nearest(&[p.x, p.x + s.x * 0.5, p.x + s.x], lines.x.iter().copied(), snap) {
            t.position.x += d;
            guides.x = Some(l);
        }
        if let Some((d, l)) = nearest(&[p.y, p.y + s.y * 0.5, p.y + s.y], lines.y.iter().copied(), snap) {
            t.position.y += d;
            guides.y = Some(l);
        }
    }
    (t.sanitized(), guides)
}

/// Draw the lines a drag snapped to across the canvas.
fn draw_guides(ui: &Ui, view: View, guides: Guides) {
    let painter = ui.painter_at(view.rect);
    let stroke = Stroke::new(1.0, GUIDE);
    if let Some(x) = guides.x {
        let sx = view.to_screen(Vec2::new(x, 0.0)).x;
        painter.line_segment([pos2(sx, view.rect.top()), pos2(sx, view.rect.bottom())], stroke);
    }
    if let Some(y) = guides.y {
        let sy = view.to_screen(Vec2::new(0.0, y)).y;
        painter.line_segment([pos2(view.rect.left(), sy), pos2(view.rect.right(), sy)], stroke);
    }
}

fn outline(ui: &Ui, view: View, corners: [Vec2; 4], color: Color32, width: f32) {
    let pts: Vec<Pos2> = corners.iter().map(|c| view.to_screen(*c)).collect();
    ui.painter_at(view.rect.expand(HANDLE)).add(egui::Shape::closed_line(pts, Stroke::new(width, color)));
}

impl Editor {
    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// The editable view: `items` bottom to top, `selected` the selected item. Interaction is off
    /// when `locked` (Lock Preview).
    pub fn show(
        &mut self,
        ui: &mut Ui,
        view: View,
        canvas: (u32, u32),
        items: &[EditItem],
        selected: Option<ItemId>,
        locked: bool,
    ) -> (EditEvent, egui::Response) {
        let mut ev = EditEvent::default();
        let resp = ui.interact(view.rect, ui.scope_id().with("edit-view"), Sense::click_and_drag());
        let accent = ui.visuals().selection.stroke.color;
        let pointer = resp.interact_pointer_pos().or_else(|| resp.hover_pos());
        let hit = |p: Pos2| -> Option<&EditItem> {
            let c = view.to_canvas(p);
            items.iter().rev().find(|it| it.visible && it.native.is_some_and(|n| it.transform.contains(n, c)))
        };
        let sel = selected.and_then(|id| items.iter().find(|it| it.id == id));

        // Hover outline.
        if self.drag.is_none()
            && let Some(p) = resp.hover_pos()
            && let Some(it) = hit(p)
            && Some(it.id) != selected
            && let Some(n) = it.native
        {
            outline(ui, view, it.transform.corners(n), accent.gamma_multiply(0.5), 1.0);
        }

        // The drag is measured from where the button was pressed.
        if resp.drag_started()
            && !locked
            && let Some(p) = ui.input(|i| i.pointer.press_origin()).or_else(|| resp.interact_pointer_pos())
        {
            let c = view.to_canvas(p);
            let corner = sel.filter(|it| !it.locked).and_then(|it| {
                let n = it.native?;
                it.transform.corners(n).iter().position(|k| view.to_screen(*k).distance(p) <= HANDLE * 1.5).map(|i| (it, n, i))
            });
            if let Some((it, n, i)) = corner {
                let lines = SnapLines::new(canvas, items, it.id);
                self.drag = Some(Drag { item: it.id, grip: Grip::Corner(i), start: c, from: it.transform, native: n, lines });
            } else if let Some(it) = hit(p) {
                if Some(it.id) != selected {
                    ev.select = Some(Some(it.id));
                }
                if !it.locked
                    && let Some(n) = it.native
                {
                    let lines = SnapLines::new(canvas, items, it.id);
                    self.drag = Some(Drag { item: it.id, grip: Grip::Move, start: c, from: it.transform, native: n, lines });
                }
            }
        }
        if let Some(d) = &self.drag
            && let Some(p) = pointer
            && resp.dragged()
        {
            let c = view.to_canvas(p);
            let (mode, alt) = ui.input(|i| (ResizeMode { free: i.modifiers.shift, centered: i.modifiers.command }, i.modifiers.alt));
            // Alt (Option on macOS) turns snapping off for as long as it is held.
            let snap = if alt { 0.0 } else { SNAP_POINTS / view.scale };
            let (t, guides) = match d.grip {
                Grip::Move => moved(d.from, d.native, Vec2::new(c.x - d.start.x, c.y - d.start.y), &d.lines, snap),
                Grip::Corner(i) => resized(d.from, d.native, i, c, mode, &d.lines, snap),
            };
            draw_guides(ui, view, guides);
            ev.transform = Some((d.item, t));
        }
        if resp.drag_stopped() && self.drag.take().is_some() {
            ev.released = true;
        }
        if resp.clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            let id = hit(p).map(|it| it.id);
            if id != selected {
                ev.select = Some(id);
            }
        }
        if resp.secondary_clicked() {
            if let Some(p) = resp.interact_pointer_pos() {
                let id = hit(p).map(|it| it.id);
                if id.is_some() && id != selected {
                    ev.select = Some(id);
                }
            }
            ev.context = true;
        }

        // The selected item, as it is now (or as the drag has it).
        if let Some(it) = sel
            && let Some(n) = it.native
        {
            let t = ev.transform.filter(|(id, _)| *id == it.id).map_or(it.transform, |(_, t)| t);
            let corners = t.corners(n);
            let color = if it.locked { Color32::from_rgb(200, 80, 80) } else { accent };
            outline(ui, view, corners, color, 2.0);
            if !it.locked && !locked {
                let painter = ui.painter_at(view.rect.expand(HANDLE));
                for k in corners {
                    let r = Rect::from_center_size(view.to_screen(k), EVec2::splat(HANDLE));
                    painter.rect_filled(r, 1.0, color);
                    painter.rect_stroke(r, 1.0, Stroke::new(1.0, Color32::BLACK), StrokeKind::Outside);
                }
            }
        }
        (ev, resp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: Vec2, b: Vec2) -> bool {
        (a.x - b.x).abs() < 0.01 && (a.y - b.y).abs() < 0.01
    }

    const KEEP: ResizeMode = ResizeMode { free: false, centered: false };
    const FREE: ResizeMode = ResizeMode { free: true, centered: false };
    const CENTER: ResizeMode = ResizeMode { free: false, centered: true };
    const CENTER_FREE: ResizeMode = ResizeMode { free: true, centered: true };

    #[test]
    fn fitting_keeps_the_canvas_shape() {
        let v = View::fit(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 800.0)), (1920, 1080));
        assert!((v.rect.width() / v.rect.height() - 16.0 / 9.0).abs() < 1e-3);
        assert!(close(v.to_canvas(v.to_screen(Vec2::new(100.0, 50.0))), Vec2::new(100.0, 50.0)));
    }

    #[test]
    fn dragging_the_bottom_right_corner_keeps_the_top_left() {
        let t = Transform { position: Vec2::new(10.0, 20.0), ..Transform::default() };
        let r = resize(t, (100, 50), 2, Vec2::new(210.0, 70.0), KEEP);
        assert!(close(r.position, Vec2::new(10.0, 20.0)), "{r:?}");
        assert!(close(r.size((100, 50)), Vec2::new(200.0, 100.0)), "aspect kept: {r:?}");
        let free = resize(t, (100, 50), 2, Vec2::new(210.0, 70.0), FREE);
        assert!(close(free.size((100, 50)), Vec2::new(200.0, 50.0)), "{free:?}");
    }

    #[test]
    fn dragging_the_top_left_corner_keeps_the_bottom_right() {
        let t = Transform { position: Vec2::new(100.0, 100.0), ..Transform::default() };
        let r = resize(t, (100, 100), 0, Vec2::new(50.0, 50.0), KEEP);
        assert!(close(r.position, Vec2::new(50.0, 50.0)), "{r:?}");
        let c = r.corners((100, 100));
        assert!(close(c[2], Vec2::new(200.0, 200.0)), "{c:?}");
    }

    #[test]
    fn a_rotated_item_keeps_its_opposite_corner() {
        let t = Transform { position: Vec2::new(100.0, 100.0), rotation: 90.0, ..Transform::default() };
        let before = t.corners((100, 40));
        let r = resize(t, (100, 40), 2, before[2], KEEP);
        let after = r.corners((100, 40));
        assert!(close(after[0], before[0]), "{before:?} {after:?}");
        let r = resize(t, (100, 40), 2, Vec2::new(before[2].x - 40.0, before[2].y + 100.0), KEEP);
        assert!(close(r.corners((100, 40))[0], before[0]), "the anchor stays when it grows");
        assert!(r.scale.x > 1.5);
    }

    #[test]
    fn resizing_from_the_center_moves_the_opposite_corner_by_the_same_amount() {
        let t = Transform { position: Vec2::new(100.0, 100.0), ..Transform::default() };
        // The bottom right corner (200, 150) goes to (250, 150); the center (150, 125) stays.
        let r = resize(t, (100, 50), 2, Vec2::new(250.0, 150.0), CENTER);
        assert!(close(r.size((100, 50)), Vec2::new(200.0, 100.0)), "aspect kept: {r:?}");
        assert!(close(r.center((100, 50)), Vec2::new(150.0, 125.0)), "{r:?}");
        assert!(close(r.position, Vec2::new(50.0, 75.0)), "{r:?}");
        let free = resize(t, (100, 50), 2, Vec2::new(250.0, 150.0), CENTER_FREE);
        assert!(close(free.size((100, 50)), Vec2::new(200.0, 50.0)), "{free:?}");
        assert!(close(free.position, Vec2::new(50.0, 100.0)), "{free:?}");
        // Dragging the top left corner inward shrinks both sides around the center.
        let r = resize(t, (100, 50), 0, Vec2::new(125.0, 112.5), CENTER);
        assert!(close(r.size((100, 50)), Vec2::new(50.0, 25.0)), "{r:?}");
        assert!(close(r.center((100, 50)), Vec2::new(150.0, 125.0)), "{r:?}");
        // Past the center the item stays a pixel across instead of turning over.
        let r = resize(t, (100, 50), 2, Vec2::new(120.0, 110.0), CENTER_FREE);
        assert!(close(r.size((100, 50)), Vec2::new(1.0, 1.0)), "{r:?}");
        assert!(close(r.center((100, 50)), Vec2::new(150.0, 125.0)), "{r:?}");
    }

    #[test]
    fn a_resize_at_the_smallest_scale_keeps_its_fixed_point() {
        // A 1920 x 1080 source can't be scaled below 0.001, so it stays 1.92 x 1.08.
        let t = Transform::default();
        let r = resize(t, (1920, 1080), 2, Vec2::new(960.0, 540.0), CENTER_FREE);
        assert!(close(r.size((1920, 1080)), Vec2::new(1.92, 1.08)), "{r:?}");
        assert!(close(r.center((1920, 1080)), Vec2::new(960.0, 540.0)), "{r:?}");
        let r = resize(t, (1920, 1080), 0, Vec2::new(1920.0, 1080.0), FREE);
        assert!(close(r.corners((1920, 1080))[2], Vec2::new(1920.0, 1080.0)), "{r:?}");
    }

    #[test]
    fn a_rotated_item_resizes_around_its_center() {
        let t = Transform { position: Vec2::new(100.0, 100.0), rotation: 90.0, ..Transform::default() };
        let c = t.center((100, 40));
        let k = t.corners((100, 40))[2];
        let r = resize(t, (100, 40), 2, Vec2::new(c.x + (k.x - c.x) * 2.0, c.y + (k.y - c.y) * 2.0), CENTER);
        assert!(close(r.center((100, 40)), c), "{r:?}");
        assert!(close(r.size((100, 40)), Vec2::new(200.0, 80.0)), "{r:?}");
    }

    fn canvas_lines() -> SnapLines {
        SnapLines::new((1920, 1080), &[], ItemId(0))
    }

    fn item(id: u64, transform: Transform, native: (u32, u32)) -> EditItem {
        EditItem { id: ItemId(id), transform, native: Some(native), locked: false, visible: true }
    }

    #[test]
    fn moving_snaps_to_edges_and_center() {
        let t = Transform::default();
        let (m, g) = moved(t, (100, 100), Vec2::new(3.0, 495.0), &canvas_lines(), 8.0);
        assert!(close(m.position, Vec2::new(0.0, 490.0)), "left edge and vertical center: {m:?}");
        assert_eq!(g, Guides { x: Some(0.0), y: Some(540.0) });
        let (free, g) = moved(t, (100, 100), Vec2::new(3.0, 500.0), &canvas_lines(), 0.0);
        assert!(close(free.position, Vec2::new(3.0, 500.0)));
        assert_eq!(g, Guides::default());
    }

    #[test]
    fn moving_snaps_to_the_nearest_line_including_other_items() {
        let other = item(2, Transform { position: Vec2::new(400.0, 300.0), ..Transform::default() }, (200, 100));
        let lines = SnapLines::new((1920, 1080), &[other], ItemId(1));
        // The left edge 395 is 5 from the other item's left edge 400.
        let (m, g) = moved(Transform::default(), (100, 100), Vec2::new(395.0, 20.0), &lines, 8.0);
        assert!(close(m.position, Vec2::new(400.0, 20.0)), "{m:?}");
        assert_eq!(g, Guides { x: Some(400.0), y: None });
        // The top edge 296 is 4 from the other item's top 300.
        let (m, g) = moved(Transform::default(), (100, 100), Vec2::new(1000.0, 296.0), &lines, 8.0);
        assert!(close(m.position, Vec2::new(1000.0, 300.0)), "{m:?}");
        assert_eq!(g, Guides { x: None, y: Some(300.0) });
    }

    #[test]
    fn resizing_snaps_an_edge_and_keeps_the_aspect_ratio() {
        let t = Transform::default();
        // Unsnapped this is 1915 x 957.5; the right edge snaps to the canvas edge.
        let (r, g) = resized(t, (100, 50), 2, Vec2::new(1915.0, 500.0), KEEP, &canvas_lines(), 8.0);
        assert!(close(r.size((100, 50)), Vec2::new(1920.0, 960.0)), "{r:?}");
        assert!(close(r.position, Vec2::ZERO), "the opposite corner stays: {r:?}");
        assert_eq!(g, Guides { x: Some(1920.0), y: None });
        // Alt turns it off.
        let (r, g) = resized(t, (100, 50), 2, Vec2::new(1915.0, 500.0), KEEP, &canvas_lines(), 0.0);
        assert!(close(r.size((100, 50)), Vec2::new(1915.0, 957.5)), "{r:?}");
        assert_eq!(g, Guides::default());
    }

    #[test]
    fn resizing_snaps_the_edge_with_the_shorter_way_and_the_other_follows() {
        let lines = SnapLines { x: vec![205.0], y: vec![203.0] };
        let (r, g) = resized(Transform::default(), (100, 100), 2, Vec2::new(202.0, 202.0), KEEP, &lines, 8.0);
        assert!(close(r.size((100, 100)), Vec2::new(203.0, 203.0)), "{r:?}");
        assert_eq!(g, Guides { x: None, y: Some(203.0) });
    }

    #[test]
    fn a_free_resize_snaps_each_edge_on_its_own() {
        let lines = SnapLines { x: vec![205.0], y: vec![103.0] };
        let (r, g) = resized(Transform::default(), (100, 100), 2, Vec2::new(202.0, 100.0), FREE, &lines, 8.0);
        assert!(close(r.size((100, 100)), Vec2::new(205.0, 103.0)), "{r:?}");
        assert_eq!(g, Guides { x: Some(205.0), y: Some(103.0) });
    }

    #[test]
    fn resizing_from_the_center_snaps_either_moving_edge_and_keeps_the_center() {
        let t = Transform { position: Vec2::new(100.0, 100.0), ..Transform::default() };
        // Unsnapped this is 210 x 210 around (150, 150), from 45 to 255; the left edge 45 is 5
        // from the line at 40, so the item becomes 220 across and the right edge goes to 260.
        let lines = SnapLines { x: vec![40.0], y: vec![] };
        let (r, g) = resized(t, (100, 100), 2, Vec2::new(255.0, 255.0), CENTER, &lines, 8.0);
        assert!(close(r.size((100, 100)), Vec2::new(220.0, 220.0)), "{r:?}");
        assert!(close(r.center((100, 100)), Vec2::new(150.0, 150.0)), "{r:?}");
        assert_eq!(g, Guides { x: Some(40.0), y: None });
        // The dragged edge snaps as well: 255 is 7 from 262.
        let lines = SnapLines { x: vec![262.0], y: vec![] };
        let (r, g) = resized(t, (100, 100), 2, Vec2::new(255.0, 255.0), CENTER, &lines, 8.0);
        assert!(close(r.size((100, 100)), Vec2::new(224.0, 224.0)), "{r:?}");
        assert!(close(r.center((100, 100)), Vec2::new(150.0, 150.0)), "{r:?}");
        assert_eq!(g, Guides { x: Some(262.0), y: None });
        // Alt turns it off.
        let (r, g) = resized(t, (100, 100), 2, Vec2::new(255.0, 255.0), CENTER, &lines, 0.0);
        assert!(close(r.size((100, 100)), Vec2::new(210.0, 210.0)), "{r:?}");
        assert_eq!(g, Guides::default());
    }

    #[test]
    fn a_free_resize_from_the_center_snaps_each_axis_on_its_own() {
        let t = Transform { position: Vec2::new(100.0, 100.0), ..Transform::default() };
        // Unsnapped this is 210 x 100 around (150, 150): the left edge 45 snaps to 40 and the
        // bottom edge 200 to 203.
        let lines = SnapLines { x: vec![40.0], y: vec![203.0] };
        let (r, g) = resized(t, (100, 100), 2, Vec2::new(255.0, 200.0), CENTER_FREE, &lines, 8.0);
        assert!(close(r.size((100, 100)), Vec2::new(220.0, 106.0)), "{r:?}");
        assert!(close(r.center((100, 100)), Vec2::new(150.0, 150.0)), "{r:?}");
        assert_eq!(g, Guides { x: Some(40.0), y: Some(203.0) });
    }

    #[test]
    fn resizing_never_snaps_to_a_line_the_scale_limits_keep_out_of_reach() {
        // An 8192 pixel wide source can't be scaled below 0.001, so it is at least 8.192 across.
        let t = Transform::default();
        let lines = SnapLines { x: vec![4096.6], y: vec![] };
        let (r, g) = resized(t, (8192, 100), 2, Vec2::new(4099.0, 53.0), CENTER_FREE, &lines, 8.0);
        assert!(close(r.size((8192, 100)), Vec2::new(8.192, 6.0)), "{r:?}");
        assert_eq!(g, Guides::default());
        let lines = SnapLines { x: vec![5.0], y: vec![] };
        let (r, g) = resized(t, (8192, 100), 2, Vec2::new(3.0, 3.0), FREE, &lines, 8.0);
        assert!(close(r.size((8192, 100)), Vec2::new(8.192, 3.0)), "{r:?}");
        assert_eq!(g, Guides::default());
    }

    #[test]
    fn a_line_on_an_edge_held_at_the_smallest_scale_still_snaps() {
        let t = Transform { position: Vec2::new(4096.0, 0.0), scale: Vec2::new(MIN_SCALE, 1.0), ..Transform::default() };
        // 4096 + 8.192 rounds to a little less than 8.192 from the fixed edge.
        let line = t.position.x + t.size((8192, 100)).x;
        let lines = SnapLines { x: vec![line], y: vec![] };
        let (r, g) = resized(t, (8192, 100), 2, Vec2::new(4106.0, 100.0), FREE, &lines, 8.0);
        assert!(close(r.size((8192, 100)), Vec2::new(8.192, 100.0)), "{r:?}");
        assert_eq!(g, Guides { x: Some(line), y: None });
    }

    #[test]
    fn resizing_from_the_center_never_snaps_onto_the_center() {
        let t = Transform { position: Vec2::new(100.0, 100.0), ..Transform::default() };
        // The smallest item is 1 x 1, so a line a fifth of a pixel from the center is ignored.
        let lines = SnapLines { x: vec![150.2], y: vec![149.9] };
        let (r, g) = resized(t, (100, 100), 2, Vec2::new(150.3, 150.3), CENTER, &lines, 8.0);
        assert!(close(r.size((100, 100)), Vec2::new(1.0, 1.0)), "{r:?}");
        assert!(close(r.center((100, 100)), Vec2::new(150.0, 150.0)), "{r:?}");
        assert_eq!(g, Guides::default());
    }

    #[test]
    fn resizing_from_the_top_left_snaps_the_top_left_edges() {
        let t = Transform { position: Vec2::new(100.0, 100.0), ..Transform::default() };
        // Dragging corner 0 to (4, 50) would put the left edge at 4; it snaps to the canvas's 0,
        // and the bottom right stays at (200, 200).
        let (r, g) = resized(t, (100, 100), 0, Vec2::new(4.0, 50.0), KEEP, &canvas_lines(), 8.0);
        assert!(close(r.position, Vec2::ZERO), "{r:?}");
        assert!(close(r.corners((100, 100))[2], Vec2::new(200.0, 200.0)), "{r:?}");
        assert_eq!(g, Guides { x: Some(0.0), y: None });
    }

    #[test]
    fn resizing_never_snaps_past_or_onto_the_fixed_corner() {
        let t = Transform { position: Vec2::new(100.0, 100.0), ..Transform::default() };
        // The smallest item is 1 x 1; a line half a pixel from the fixed corner is ignored.
        let lines = SnapLines { x: vec![100.5, 99.0], y: vec![] };
        let (r, g) = resized(t, (100, 100), 2, Vec2::new(100.2, 100.2), KEEP, &lines, 8.0);
        assert!(close(r.size((100, 100)), Vec2::new(1.0, 1.0)), "{r:?}");
        assert_eq!(g, Guides::default());
    }

    #[test]
    fn a_rotated_item_resizes_without_snapping() {
        let t = Transform { position: Vec2::new(100.0, 100.0), rotation: 30.0, ..Transform::default() };
        let to = t.corners((100, 100))[2];
        let (r, g) = resized(t, (100, 100), 2, to, KEEP, &canvas_lines(), 1e6);
        assert_eq!(r, resize(t, (100, 100), 2, to, KEEP));
        let (r, g2) = resized(t, (100, 100), 2, to, CENTER, &canvas_lines(), 1e6);
        assert_eq!(r, resize(t, (100, 100), 2, to, CENTER));
        assert_eq!(g2, Guides::default());
        assert_eq!(g, Guides::default());
    }

    #[test]
    fn snap_lines_hold_the_canvas_and_the_other_shown_items() {
        let hidden = EditItem { visible: false, ..item(3, Transform::default(), (10, 10)) };
        let waiting = EditItem { native: None, ..item(4, Transform::default(), (10, 10)) };
        let tilted = item(5, Transform { rotation: 45.0, ..Transform::default() }, (10, 10));
        // Turned a quarter around its center (50, 20): it covers 30..70 by -30..70.
        let turned = item(6, Transform { position: Vec2::ZERO, rotation: 90.0, ..Transform::default() }, (100, 40));
        let dragged = item(1, Transform::default(), (10, 10));
        let lines = SnapLines::new((1920, 1080), &[hidden, waiting, tilted, turned, dragged], ItemId(1));
        assert_eq!(lines.x.len(), 10, "{lines:?}");
        assert!(close(Vec2::new(lines.x[3], lines.y[3]), Vec2::new(30.0, -30.0)), "{lines:?}");
        assert!(close(Vec2::new(lines.x[5], lines.y[5]), Vec2::new(70.0, 70.0)), "{lines:?}");
        assert_eq!(&lines.x[..3], &[0.0, 960.0, 1920.0]);
        assert_eq!(&lines.y[..3], &[0.0, 540.0, 1080.0]);
    }

    #[test]
    fn moves_and_resizes_snap_to_the_safe_areas() {
        let lines = canvas_lines();
        // Action safe is 3.5% in from each edge, title safe 5%: 67.2 and 96 across, 37.8 and 54 down.
        for (x, y) in [(67.2, 37.8), (96.0, 54.0), (1852.8, 1042.2), (1824.0, 1026.0)] {
            assert!(lines.x.iter().any(|l| (l - x).abs() < 0.01) && lines.y.iter().any(|l| (l - y).abs() < 0.01), "{x}, {y}: {lines:?}");
        }
        let (m, g) = moved(Transform::default(), (100, 100), Vec2::new(92.0, 300.0), &lines, 8.0);
        assert!(close(m.position, Vec2::new(96.0, 300.0)), "{m:?}");
        assert_eq!(g, Guides { x: Some(96.0), y: None });
        // The bottom right corner of a 100 x 50 item snaps to the title safe area's right edge.
        let (r, g) = resized(Transform::default(), (100, 50), 2, Vec2::new(1820.0, 600.0), KEEP, &lines, 8.0);
        assert!(close(r.size((100, 50)), Vec2::new(1824.0, 912.0)), "{r:?}");
        assert_eq!(g, Guides { x: Some(1824.0), y: None });
    }
}
