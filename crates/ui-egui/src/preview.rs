//! The preview and program views: the compositor's canvas textures drawn in the window, and the
//! handles that move and resize the selected item. Moves and resizes snap to the canvas's edges,
//! center lines and safe areas and to other items' edges and center lines, which the drag holds
//! as [`SnapTargets`]; a move follows the pointer through a [`MoveGesture`]. Alt (Option on macOS)
//! turns snapping off while it is held ([`SnapSuspend::Alt`]).

use craft_behavior::Axis;
use craft_behavior::geometry::{ConstrainStyle, Directions, MoveGesture, SNAP_PX, SnapKind, SnapSuspend, SnapTarget, SnapTargets};
use craft_behavior::kurbo;
use craft_keymap::Mods;
use egui::{Color32, Pos2, Rect, Sense, Stroke, StrokeKind, TextureId, Ui, Vec2 as EVec2, pos2, vec2};
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

#[derive(Clone, Debug, PartialEq)]
enum Grip {
    Move(MoveGesture),
    /// Clockwise from the top left.
    Corner(usize),
}

#[derive(Clone, Debug)]
struct Drag {
    item: ItemId,
    grip: Grip,
    from: Transform,
    native: (u32, u32),
    /// What the dragged item's edges snap to, gathered when the drag starts.
    targets: SnapTargets,
    /// The transform the drag last reported, so a frame that changes nothing reports nothing.
    last: Option<Transform>,
}

/// What a dragged item's edges and center snap to: the canvas's edges and center lines, the
/// edges and center lines of every other visible item that has shown something and is turned by
/// a multiple of 90 degrees, and the edges of the canvas's safe areas ([`SAFE_MARGINS`], as
/// [`SnapKind::Guide`]). Each line runs the full length of the canvas. Only the lines `keep`
/// accepts are targets; it is given the axis a line runs along (`Axis::Y` for a vertical line)
/// and its position across it, in canvas pixels.
pub fn snap_targets(canvas: (u32, u32), items: &[EditItem], dragged: ItemId, keep: impl Fn(Axis, f32) -> bool) -> SnapTargets {
    let mut targets = SnapTargets::new();
    let mut add = |min: Vec2, max: Vec2, edge: SnapKind, center: SnapKind| {
        for (along, lo, hi) in [(Axis::Y, min.x, max.x), (Axis::X, min.y, max.y)] {
            for (at, kind) in [(lo, edge), ((lo + hi) * 0.5, center), (hi, edge)] {
                if keep(along, at) {
                    targets.add(SnapTarget::new(along, f64::from(at), kind));
                }
            }
        }
    };
    add(Vec2::ZERO, Vec2::new(canvas.0 as f32, canvas.1 as f32), SnapKind::Bounds, SnapKind::Center);
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
        add(min, max, SnapKind::LayerEdge, SnapKind::LayerCenter);
    }
    let (w, h) = (canvas.0 as f32, canvas.1 as f32);
    for m in SAFE_MARGINS {
        for (along, at) in [(Axis::Y, w * m), (Axis::Y, w * (1.0 - m)), (Axis::X, h * m), (Axis::X, h * (1.0 - m))] {
            if keep(along, at) {
                targets.add(SnapTarget::new(along, f64::from(at), SnapKind::Guide));
            }
        }
    }
    targets
}

/// The edges of the canvas's safe areas, as a share of each side measured from its edges: the
/// action safe area and the title safe area.
pub const SAFE_MARGINS: [f32; 2] = [0.035, 0.05];

/// The lines a drag of corner `corner` of `from` may snap its moving edges to: a line on the far
/// side of the fixed corner, or less than a pixel from it, would turn the item over. For
/// [`snap_targets`]'s `keep`.
pub fn ahead_of_fixed(from: Transform, native: (u32, u32), corner: usize) -> impl Fn(Axis, f32) -> bool {
    let (a, dir) = anchor(corner, from.size(native));
    let fixed = Vec2::new(from.position.x + a.x, from.position.y + a.y);
    move |along, at| if along == Axis::Y { (at - fixed.x) * dir.0 >= 1.0 } else { (at - fixed.y) * dir.1 >= 1.0 }
}

/// A line a drag snapped to this frame, drawn as a guide across the canvas: a vertical line
/// (along `Axis::Y`) at an x, or a horizontal one (along `Axis::X`) at a y, in canvas pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Guide {
    pub along: Axis,
    pub at: f32,
}

/// A canvas point as the framework's geometry takes it.
fn point(p: Vec2) -> kurbo::Point {
    kurbo::Point::new(f64::from(p.x), f64::from(p.y))
}

/// The target nearest to `edge` among the lines running `along`, when it is at most `snap`
/// away: the distance to move by and the line.
fn nearest(targets: &SnapTargets, along: Axis, edge: f32, snap: f32) -> Option<(f32, f32)> {
    let t = targets.nearest(along, f64::from(edge), f64::from(snap))?;
    let at = t.position as f32;
    Some((at - edge, at))
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

/// The color of the lines a drag snapped to.
const GUIDE: Color32 = Color32::from_rgb(255, 64, 160);
const HANDLE: f32 = 7.0;

#[derive(Default)]
pub struct Editor {
    drag: Option<Drag>,
}

/// The corner opposite `i` and the direction from it to corner `i`, in the item's own box.
fn anchor(i: usize, size: Vec2) -> (Vec2, (f32, f32)) {
    match i {
        0 => (Vec2::new(size.x, size.y), (-1.0, -1.0)),
        1 => (Vec2::new(0.0, size.y), (1.0, -1.0)),
        2 => (Vec2::new(0.0, 0.0), (1.0, 1.0)),
        _ => (Vec2::new(size.x, 0.0), (-1.0, 1.0)),
    }
}

/// `from` resized by dragging corner `corner` to the canvas point `to`, keeping the opposite
/// corner in place (and the aspect ratio unless `free`).
pub fn resize(from: Transform, native: (u32, u32), corner: usize, to: Vec2, free: bool) -> Transform {
    let size = from.size(native);
    if size.x <= 0.0 || size.y <= 0.0 {
        return from;
    }
    let p = from.to_local(native, to);
    let (a, dir) = anchor(corner, size);
    let mut new = Vec2::new(((p.x - a.x) * dir.0).max(1.0), ((p.y - a.y) * dir.1).max(1.0));
    if !free {
        let k = (new.x / size.x).max(new.y / size.y);
        new = Vec2::new(size.x * k, size.y * k);
    }
    let min = Vec2::new(if dir.0 < 0.0 { a.x - new.x } else { a.x }, if dir.1 < 0.0 { a.y - new.y } else { a.y });
    // The new box's center, from the item's own box to the canvas.
    let local = Vec2::new(min.x + new.x * 0.5 - size.x * 0.5, min.y + new.y * 0.5 - size.y * 0.5);
    let c0 = from.center(native);
    let (s, co) = from.rotation.to_radians().sin_cos();
    let c = Vec2::new(c0.x + local.x * co - local.y * s, c0.y + local.x * s + local.y * co);
    let t = from.with_size(native, new);
    Transform { position: Vec2::new(c.x - new.x * 0.5, c.y - new.y * 0.5), ..t }.sanitized()
}

/// [`resize`], with the moving edges snapped to `targets` when an edge of an unrotated item comes
/// within `snap` canvas pixels of one. With the aspect ratio kept, the edge that has the shorter
/// way to its line snaps and the other edge follows the ratio; with `free`, each edge snaps on
/// its own. The opposite corner stays in place. `targets` holds only the lines
/// [`ahead_of_fixed`] keeps.
pub fn resized(
    from: Transform,
    native: (u32, u32),
    corner: usize,
    to: Vec2,
    free: bool,
    targets: &SnapTargets,
    snap: f32,
) -> (Transform, Vec<Guide>) {
    let r = resize(from, native, corner, to, free);
    let size = from.size(native);
    if snap <= 0.0 || from.rotation != 0.0 || size.x <= 0.0 || size.y <= 0.0 {
        return (r, Vec::new());
    }
    let (a, dir) = anchor(corner, size);
    let fixed = Vec2::new(from.position.x + a.x, from.position.y + a.y);
    let now = r.size(native);
    let edge = Vec2::new(fixed.x + dir.0 * now.x, fixed.y + dir.1 * now.y);
    let sx = nearest(targets, Axis::Y, edge.x, snap);
    let sy = nearest(targets, Axis::X, edge.y, snap);
    let corner_at = |w: f32, h: f32| Vec2::new(fixed.x + dir.0 * w, fixed.y + dir.1 * h);
    let vertical = |at: f32| Guide { along: Axis::Y, at };
    let horizontal = |at: f32| Guide { along: Axis::X, at };
    if free {
        if sx.is_none() && sy.is_none() {
            return (r, Vec::new());
        }
        let w = sx.map_or(now.x, |(_, l)| (l - fixed.x).abs());
        let h = sy.map_or(now.y, |(_, l)| (l - fixed.y).abs());
        let guides = sx.map(|s| vertical(s.1)).into_iter().chain(sy.map(|s| horizontal(s.1))).collect();
        return (resize(from, native, corner, corner_at(w, h), true), guides);
    }
    let (k, guide) = match (sx, sy) {
        (Some((dx, l)), Some((dy, _))) if dx.abs() <= dy.abs() => ((l - fixed.x).abs() / size.x, vertical(l)),
        (_, Some((_, l))) => ((l - fixed.y).abs() / size.y, horizontal(l)),
        (Some((_, l)), None) => ((l - fixed.x).abs() / size.x, vertical(l)),
        (None, None) => return (r, Vec::new()),
    };
    (resize(from, native, corner, corner_at(size.x * k, size.y * k), false), vec![guide])
}

/// `from` moved by `delta`, snapped to `targets` when an edge or the center line of an unrotated
/// item comes within `snap` canvas pixels of one. Each axis snaps by the shortest way any of the
/// item's lines has to a target, and every line of the item that then lies on a target is a guide.
pub fn moved(from: Transform, native: (u32, u32), delta: Vec2, targets: &SnapTargets, snap: f32) -> (Transform, Vec<Guide>) {
    let mut t = from;
    t.position = Vec2::new(from.position.x + delta.x, from.position.y + delta.y);
    let mut guides = Vec::new();
    if snap > 0.0 && from.rotation == 0.0 {
        let (p, s) = (t.position, t.size(native));
        let rect = kurbo::Rect::new(f64::from(p.x), f64::from(p.y), f64::from(p.x + s.x), f64::from(p.y + s.y));
        let (d, lines) = targets.snap_rect(rect, f64::from(snap));
        t.position = Vec2::new(p.x + d.x as f32, p.y + d.y as f32);
        guides.extend(lines.iter().map(|l| Guide { along: l.along, at: l.position as f32 }));
    }
    (t.sanitized(), guides)
}

/// Draw the lines a drag snapped to across the canvas.
fn draw_guides(ui: &Ui, view: View, guides: &[Guide]) {
    let painter = ui.painter_at(view.rect);
    let stroke = Stroke::new(1.0, GUIDE);
    for g in guides {
        if g.along == Axis::Y {
            let sx = view.to_screen(Vec2::new(g.at, 0.0)).x;
            painter.line_segment([pos2(sx, view.rect.top()), pos2(sx, view.rect.bottom())], stroke);
        } else {
            let sy = view.to_screen(Vec2::new(0.0, g.at)).y;
            painter.line_segment([pos2(view.rect.left(), sy), pos2(view.rect.right(), sy)], stroke);
        }
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
                let targets = snap_targets(canvas, items, it.id, ahead_of_fixed(it.transform, n, i));
                self.drag = Some(Drag { item: it.id, grip: Grip::Corner(i), from: it.transform, native: n, targets, last: None });
            } else if let Some(it) = hit(p) {
                if Some(it.id) != selected {
                    ev.select = Some(Some(it.id));
                }
                if !it.locked
                    && let Some(n) = it.native
                {
                    let targets = snap_targets(canvas, items, it.id, |_, _| true);
                    // The gesture is given no keys: Shift and Alt keep this view's meanings (a
                    // free resize, snapping off), so a move neither locks its direction nor copies.
                    let gesture = MoveGesture::press(&ConstrainStyle::new(0.0, 0.0), point(c), Mods::NONE, Directions::Four);
                    self.drag = Some(Drag { item: it.id, grip: Grip::Move(gesture), from: it.transform, native: n, targets, last: None });
                }
            }
        }
        if let Some(d) = &mut self.drag
            && let Some(p) = pointer
            && resp.dragged()
        {
            let c = view.to_canvas(p);
            let modifiers = ui.input(|i| i.modifiers);
            let platform = craft_keymap::egui::platform(ui.ctx());
            // Alt (Option on macOS) turns snapping off for as long as it is held.
            let suspended = SnapSuspend::Alt.is_held(Mods::from_egui(&modifiers, platform), platform);
            let snap = if suspended { 0.0 } else { SNAP_PX as f32 / view.scale };
            let (t, guides) = match &mut d.grip {
                Grip::Move(gesture) => {
                    gesture.follow(point(c), Mods::NONE, f64::from(view.scale));
                    let delta = gesture.value().delta;
                    moved(d.from, d.native, Vec2::new(delta.x as f32, delta.y as f32), &d.targets, snap)
                }
                Grip::Corner(i) => resized(d.from, d.native, *i, c, modifiers.shift, &d.targets, snap),
            };
            draw_guides(ui, view, &guides);
            if d.last != Some(t) {
                d.last = Some(t);
                ev.transform = Some((d.item, t));
            }
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
            let dragged = self.drag.as_ref().and_then(|d| Some((d.item, d.last?)));
            let t = ev.transform.or(dragged).filter(|(id, _)| *id == it.id).map_or(it.transform, |(_, t)| t);
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
    use craft_behavior::geometry::SnapKinds;

    fn close(a: Vec2, b: Vec2) -> bool {
        (a.x - b.x).abs() < 0.01 && (a.y - b.y).abs() < 0.01
    }

    fn item(id: u64, transform: Transform, native: (u32, u32)) -> EditItem {
        EditItem { id: ItemId(id), transform, native: Some(native), locked: false, visible: true }
    }

    fn at(x: f32, y: f32) -> Transform {
        Transform { position: Vec2::new(x, y), ..Transform::default() }
    }

    /// The canvas's lines alone, with `keep`.
    fn canvas(keep: impl Fn(Axis, f32) -> bool) -> SnapTargets {
        snap_targets((1920, 1080), &[], ItemId(0), keep)
    }

    /// Vertical lines at `x` and horizontal lines at `y`, with `keep`.
    fn lines(x: &[f32], y: &[f32], keep: impl Fn(Axis, f32) -> bool) -> SnapTargets {
        let mut targets = SnapTargets::new();
        for (along, all) in [(Axis::Y, x), (Axis::X, y)] {
            for p in all.iter().copied().filter(|p| keep(along, *p)) {
                targets.add(SnapTarget::new(along, f64::from(p), SnapKind::Guide));
            }
        }
        targets
    }

    fn v(at: f32) -> Guide {
        Guide { along: Axis::Y, at }
    }

    fn h(at: f32) -> Guide {
        Guide { along: Axis::X, at }
    }

    #[test]
    fn a_corner_drag_keeps_the_opposite_corner() {
        let turned = Transform { position: Vec2::new(100.0, 100.0), rotation: 90.0, ..Transform::default() };
        let corner = turned.corners((100, 40))[2];
        // (what, from, native, corner, to, free, size after)
        let rows = [
            ("bottom right, ratio kept", at(10.0, 20.0), (100, 50), 2, Vec2::new(210.0, 70.0), false, Vec2::new(200.0, 100.0)),
            ("bottom right, Shift frees the ratio", at(10.0, 20.0), (100, 50), 2, Vec2::new(210.0, 70.0), true, Vec2::new(200.0, 50.0)),
            ("top left", at(100.0, 100.0), (100, 100), 0, Vec2::new(50.0, 50.0), false, Vec2::new(150.0, 150.0)),
            ("a quarter turn, the corner where it was", turned, (100, 40), 2, corner, false, Vec2::new(100.0, 40.0)),
            ("a quarter turn, grown", turned, (100, 40), 2, Vec2::new(corner.x - 40.0, corner.y + 100.0), false, Vec2::new(200.0, 80.0)),
            ("past the fixed corner: 1 x 1 at the least", at(0.0, 0.0), (100, 100), 2, Vec2::new(-50.0, -50.0), true, Vec2::new(1.0, 1.0)),
        ];
        for (what, from, native, corner, to, free, size) in rows {
            let r = resize(from, native, corner, to, free);
            assert!(close(r.size(native), size), "{what}: {r:?}");
            let opposite = (corner + 2) % 4;
            assert!(close(r.corners(native)[opposite], from.corners(native)[opposite]), "{what}: {r:?}");
        }
    }

    #[test]
    fn a_move_snaps_each_axis_to_the_canvas_and_other_items() {
        let other = item(2, at(400.0, 300.0), (200, 100));
        let with_other = snap_targets((1920, 1080), &[other], ItemId(1), |_, _| true);
        // (what, targets, delta, snap, position after, guides)
        let rows = [
            ("to the canvas edge and center line", canvas(|_, _| true), Vec2::new(3.0, 495.0), 8.0, Vec2::new(0.0, 490.0), vec![v(0.0), h(540.0)]),
            ("Alt: no snapping", canvas(|_, _| true), Vec2::new(3.0, 500.0), 0.0, Vec2::new(3.0, 500.0), vec![]),
            // The left edge 395 is 5 from the other item's left edge 400; the right edge then lies
            // on its center line 500.
            ("left edge to another item's", with_other.clone(), Vec2::new(395.0, 20.0), 8.0, Vec2::new(400.0, 20.0), vec![v(400.0), v(500.0)]),
            // The top edge 296 is 4 from the other item's top 300; the bottom and the center then
            // lie on its bottom 400 and center line 350.
            ("top edge to another item's", with_other, Vec2::new(1000.0, 296.0), 8.0, Vec2::new(1000.0, 300.0), vec![h(300.0), h(400.0), h(350.0)]),
        ];
        for (what, targets, delta, snap, position, guides) in rows {
            let (m, g) = moved(Transform::default(), (100, 100), delta, &targets, snap);
            assert!(close(m.position, position), "{what}: {m:?}");
            assert_eq!(g, guides, "{what}");
        }
    }

    #[test]
    fn a_corner_drag_snaps_the_edges_it_moves() {
        let p = Vec2::new;
        let (from, tl) = (Transform::default(), at(100.0, 100.0));
        // (from, native, corner, lines (the canvas's when `None`), to, free, snap, size after, guides)
        let rows = [
            // 0: the right edge snaps to the canvas edge and the ratio is kept. Unsnapped this is
            // 1915 x 957.5.
            (from, (100, 50), 2, None, p(1915.0, 500.0), false, 8.0, p(1920.0, 960.0), vec![v(1920.0)]),
            // 1: Alt, no snapping.
            (from, (100, 50), 2, None, p(1915.0, 500.0), false, 0.0, p(1915.0, 957.5), vec![]),
            // 2: the edge with the shorter way snaps and the other follows the ratio.
            (from, (100, 100), 2, Some((vec![205.0], vec![203.0])), p(202.0, 202.0), false, 8.0, p(203.0, 203.0), vec![h(203.0)]),
            // 3: Shift, each edge snaps on its own.
            (from, (100, 100), 2, Some((vec![205.0], vec![103.0])), p(202.0, 100.0), true, 8.0, p(205.0, 103.0), vec![v(205.0), h(103.0)]),
            // 4: corner 0 at (4, 50) would put the left edge at 4; it snaps to the canvas's 0.
            (tl, (100, 100), 0, None, p(4.0, 50.0), false, 8.0, p(200.0, 200.0), vec![v(0.0)]),
            // 5: a line half a pixel from the fixed corner, or behind it, is no target.
            (tl, (100, 100), 2, Some((vec![100.5, 99.0], vec![])), p(100.2, 100.2), false, 8.0, p(1.0, 1.0), vec![]),
        ];
        for (row, (from, native, corner, given, to, free, snap, size, guides)) in rows.into_iter().enumerate() {
            let keep = ahead_of_fixed(from, native, corner);
            let targets = match given {
                Some((x, y)) => lines(&x, &y, keep),
                None => canvas(keep),
            };
            let (r, g) = resized(from, native, corner, to, free, &targets, snap);
            assert!(close(r.size(native), size), "row {row}: {r:?}");
            let opposite = (corner + 2) % 4;
            assert!(close(r.corners(native)[opposite], from.corners(native)[opposite]), "row {row}: the opposite corner stays: {r:?}");
            assert_eq!(g, guides, "row {row}");
        }
    }

    #[test]
    fn a_rotated_item_resizes_without_snapping() {
        let t = Transform { position: Vec2::new(100.0, 100.0), rotation: 30.0, ..Transform::default() };
        let to = t.corners((100, 100))[2];
        let (r, g) = resized(t, (100, 100), 2, to, false, &canvas(|_, _| true), 1e6);
        assert_eq!(r, resize(t, (100, 100), 2, to, false));
        assert_eq!(g, vec![]);
    }

    #[test]
    fn snap_targets_hold_the_canvas_and_the_other_shown_items() {
        let hidden = EditItem { visible: false, ..item(3, Transform::default(), (10, 10)) };
        let waiting = EditItem { native: None, ..item(4, Transform::default(), (10, 10)) };
        let tilted = item(5, Transform { rotation: 45.0, ..Transform::default() }, (10, 10));
        // Turned a quarter around its center (50, 20): it covers 30..70 by -30..70.
        let turned = item(6, Transform { position: Vec2::ZERO, rotation: 90.0, ..Transform::default() }, (100, 40));
        let dragged = item(1, Transform::default(), (10, 10));
        let targets = snap_targets((1920, 1080), &[hidden, waiting, tilted, turned, dragged], ItemId(1), |_, _| true);
        // The canvas's lines, and the turned item's; the hidden, waiting, tilted and dragged items,
        // all at 0..10, give no item lines at 5 and 10.
        let rows = [
            (Axis::Y, 0.0, SnapKind::Bounds, true),
            (Axis::Y, 960.0, SnapKind::Center, true),
            (Axis::Y, 1920.0, SnapKind::Bounds, true),
            (Axis::X, 0.0, SnapKind::Bounds, true),
            (Axis::X, 540.0, SnapKind::Center, true),
            (Axis::X, 1080.0, SnapKind::Bounds, true),
            (Axis::Y, 30.0, SnapKind::LayerEdge, true),
            (Axis::Y, 50.0, SnapKind::LayerCenter, true),
            (Axis::Y, 70.0, SnapKind::LayerEdge, true),
            (Axis::X, -30.0, SnapKind::LayerEdge, true),
            (Axis::X, 20.0, SnapKind::LayerCenter, true),
            (Axis::X, 70.0, SnapKind::LayerEdge, true),
            (Axis::Y, 5.0, SnapKind::LayerCenter, false),
            (Axis::Y, 10.0, SnapKind::LayerEdge, false),
            (Axis::X, 5.0, SnapKind::LayerCenter, false),
            (Axis::X, 10.0, SnapKind::LayerEdge, false),
        ];
        for (along, position, kind, held) in rows {
            let found = targets.nearest_of(along, position, 0.01, SnapKinds::only(kind));
            assert_eq!(found.is_some(), held, "{along:?} {position} {kind:?}: {found:?}");
        }
        // `keep` drops lines: here every vertical line.
        let horizontal = snap_targets((1920, 1080), &[turned], ItemId(1), |along, _| along == Axis::X);
        assert_eq!(horizontal.nearest(Axis::Y, 0.0, 1e6), None);
        assert!(horizontal.nearest(Axis::X, 21.0, 2.0).is_some_and(|t| t.position == 20.0));
    }

    #[test]
    fn a_drag_in_the_view_snaps_and_alt_turns_snapping_off() {
        use std::cell::RefCell;
        use std::rc::Rc;
        // A 16:9 canvas fitted in a square area keeps its shape: 400 / 1920 points per pixel.
        let view = View::fit(Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 400.0)), (1920, 1080));
        assert!((view.rect.width() / view.rect.height() - 16.0 / 9.0).abs() < 1e-3);
        assert!(close(view.to_canvas(view.to_screen(Vec2::new(100.0, 50.0))), Vec2::new(100.0, 50.0)));
        let it = item(1, at(100.0, 100.0), (192, 108));
        let seen: Rc<RefCell<Vec<EditEvent>>> = Rc::default();
        let mut editor = Editor::default();
        let mut f = {
            let seen = Rc::clone(&seen);
            craft_ui::testing::Fixture::new(move |ui| {
                let (ev, _) = editor.show(ui, view, (1920, 1080), &[it], Some(ItemId(1)), false);
                seen.borrow_mut().push(ev);
            })
        };
        f.room(vec2(400.0, 400.0));
        let center = view.to_screen(Vec2::new(196.0, 154.0));
        // 90 pixels to the left puts the left edge at 10, inside the 8 points (38.4 pixels) the
        // canvas edge snaps from.
        let by = vec2(-90.0 * view.scale, 0.0);
        let last = |seen: &RefCell<Vec<EditEvent>>| {
            let events = seen.take();
            assert!(events.iter().any(|e| e.released), "the drag ends");
            events.into_iter().rev().find_map(|e| e.transform)
        };
        f.drag_at(center, by);
        let (id, t) = last(&seen).unwrap_or_else(|| panic!("the drag moves the item"));
        assert_eq!(id, ItemId(1));
        assert!(close(t.position, Vec2::new(0.0, 100.0)), "it snaps to the canvas edge: {t:?}");
        f.hold(egui::Modifiers::ALT).drag_at(center, by);
        let (_, t) = last(&seen).unwrap_or_else(|| panic!("the drag moves the item"));
        assert!(close(t.position, Vec2::new(10.0, 100.0)), "Alt turns snapping off: {t:?}");
    }

    #[test]
    fn moves_and_resizes_snap_to_the_safe_areas() {
        let targets = canvas(|_, _| true);
        // Action safe is 3.5% in from each edge, title safe 5%: 67.2 and 96 across, 37.8 and 54 down.
        for (x, y) in [(67.2, 37.8), (96.0, 54.0), (1852.8, 1042.2), (1824.0, 1026.0)] {
            let guide = SnapKinds::only(SnapKind::Guide);
            assert!(targets.nearest_of(Axis::Y, x, 0.01, guide).is_some(), "a vertical line at {x}");
            assert!(targets.nearest_of(Axis::X, y, 0.01, guide).is_some(), "a horizontal line at {y}");
        }
        let (m, g) = moved(Transform::default(), (100, 100), Vec2::new(92.0, 300.0), &targets, 8.0);
        assert!(close(m.position, Vec2::new(96.0, 300.0)), "{m:?}");
        assert_eq!(g, vec![v(96.0)]);
        // The bottom right corner of a 100 x 50 item snaps to the title safe area's right edge.
        let keep = ahead_of_fixed(Transform::default(), (100, 50), 2);
        let (r, g) = resized(Transform::default(), (100, 50), 2, Vec2::new(1820.0, 600.0), false, &canvas(keep), 8.0);
        assert!(close(r.size((100, 50)), Vec2::new(1824.0, 912.0)), "{r:?}");
        assert_eq!(g, vec![v(1824.0)]);
    }
}
