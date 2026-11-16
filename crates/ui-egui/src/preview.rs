//! The preview and program views: the compositor's canvas textures drawn in the window, and the
//! handles that move and resize the selected item.

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

#[derive(Clone, Copy, Debug, PartialEq)]
enum Grip {
    Move,
    /// Clockwise from the top left.
    Corner(usize),
}

#[derive(Clone, Copy, Debug)]
struct Drag {
    item: ItemId,
    grip: Grip,
    start: Vec2,
    from: Transform,
    native: (u32, u32),
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

/// Screen distance at which a moved item snaps to the canvas edges and center.
const SNAP_POINTS: f32 = 8.0;
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

/// `from` moved by `delta`, snapped to the canvas's edges and center when an unrotated edge or
/// the center comes within `snap` canvas pixels.
pub fn moved(from: Transform, native: (u32, u32), delta: Vec2, canvas: (u32, u32), snap: f32) -> Transform {
    let mut t = from;
    t.position = Vec2::new(from.position.x + delta.x, from.position.y + delta.y);
    if snap > 0.0 && from.rotation == 0.0 {
        let size = t.size(native);
        let snap_axis = |pos: f32, len: f32, total: f32| -> f32 {
            let candidates = [(pos, 0.0), (pos + len, total), (pos + len * 0.5, total * 0.5)];
            candidates.iter().filter(|(edge, target)| (edge - target).abs() <= snap).map(|(edge, target)| pos + (target - edge)).next().unwrap_or(pos)
        };
        t.position = Vec2::new(snap_axis(t.position.x, size.x, canvas.0 as f32), snap_axis(t.position.y, size.y, canvas.1 as f32));
    }
    t.sanitized()
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
                self.drag = Some(Drag { item: it.id, grip: Grip::Corner(i), start: c, from: it.transform, native: n });
            } else if let Some(it) = hit(p) {
                if Some(it.id) != selected {
                    ev.select = Some(Some(it.id));
                }
                if !it.locked
                    && let Some(n) = it.native
                {
                    self.drag = Some(Drag { item: it.id, grip: Grip::Move, start: c, from: it.transform, native: n });
                }
            }
        }
        if let Some(d) = self.drag
            && let Some(p) = pointer
            && resp.dragged()
        {
            let c = view.to_canvas(p);
            let free = ui.input(|i| i.modifiers.shift);
            let t = match d.grip {
                Grip::Move => {
                    let snap = if ui.input(|i| i.modifiers.alt) { 0.0 } else { SNAP_POINTS / view.scale };
                    moved(d.from, d.native, Vec2::new(c.x - d.start.x, c.y - d.start.y), canvas, snap)
                }
                Grip::Corner(i) => resize(d.from, d.native, i, c, free),
            };
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

    #[test]
    fn fitting_keeps_the_canvas_shape() {
        let v = View::fit(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 800.0)), (1920, 1080));
        assert!((v.rect.width() / v.rect.height() - 16.0 / 9.0).abs() < 1e-3);
        assert!(close(v.to_canvas(v.to_screen(Vec2::new(100.0, 50.0))), Vec2::new(100.0, 50.0)));
    }

    #[test]
    fn dragging_the_bottom_right_corner_keeps_the_top_left() {
        let t = Transform { position: Vec2::new(10.0, 20.0), ..Transform::default() };
        let r = resize(t, (100, 50), 2, Vec2::new(210.0, 70.0), false);
        assert!(close(r.position, Vec2::new(10.0, 20.0)), "{r:?}");
        assert!(close(r.size((100, 50)), Vec2::new(200.0, 100.0)), "aspect kept: {r:?}");
        let free = resize(t, (100, 50), 2, Vec2::new(210.0, 70.0), true);
        assert!(close(free.size((100, 50)), Vec2::new(200.0, 50.0)), "{free:?}");
    }

    #[test]
    fn dragging_the_top_left_corner_keeps_the_bottom_right() {
        let t = Transform { position: Vec2::new(100.0, 100.0), ..Transform::default() };
        let r = resize(t, (100, 100), 0, Vec2::new(50.0, 50.0), false);
        assert!(close(r.position, Vec2::new(50.0, 50.0)), "{r:?}");
        let c = r.corners((100, 100));
        assert!(close(c[2], Vec2::new(200.0, 200.0)), "{c:?}");
    }

    #[test]
    fn a_rotated_item_keeps_its_opposite_corner() {
        let t = Transform { position: Vec2::new(100.0, 100.0), rotation: 90.0, ..Transform::default() };
        let before = t.corners((100, 40));
        let r = resize(t, (100, 40), 2, before[2], false);
        let after = r.corners((100, 40));
        assert!(close(after[0], before[0]), "{before:?} {after:?}");
        let r = resize(t, (100, 40), 2, Vec2::new(before[2].x - 40.0, before[2].y + 100.0), false);
        assert!(close(r.corners((100, 40))[0], before[0]), "the anchor stays when it grows");
        assert!(r.scale.x > 1.5);
    }

    #[test]
    fn moving_snaps_to_edges_and_center() {
        let t = Transform::default();
        let m = moved(t, (100, 100), Vec2::new(3.0, 495.0), (1920, 1080), 8.0);
        assert!(close(m.position, Vec2::new(0.0, 490.0)), "left edge and vertical center: {m:?}");
        let free = moved(t, (100, 100), Vec2::new(3.0, 500.0), (1920, 1080), 0.0);
        assert!(close(free.position, Vec2::new(3.0, 500.0)));
    }
}
