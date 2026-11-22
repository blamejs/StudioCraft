//! A scene as a tile of a grid: its picture over its name, with its shortcut on a key at the
//! picture's top start corner and its program and preview chips at the top end corner. A chip
//! that does not fit inside the picture, or a key that does not fit in the room the chips leave,
//! is not drawn.
//!
//! A tile in the program has a 2-point border in the theme's error solid. A tile that is selected
//! or in the preview, and not in the program, has a 2-point border in the theme's accent solid.
//! Any other tile has a 1-point border in the theme's border color. Until its first picture is
//! drawn, a tile shows the sunken surface in its place. The name is shortened with an ellipsis
//! when it is wider than the tile. A pointer over the tile brightens the picture over the theme's
//! hover time, which is 0 when the suite asks for reduced motion, so the change is then
//! immediate. A focused tile has the theme's focus ring. A screen reader reads the tile as a
//! button with the name the caller gives it.
//!
//! An add tile has the same size and border and shows a plus over its label.
//!
//! In a right-to-left interface the key is at the top right, the chips at the top left and the
//! name is aligned to the right.

use craft_ui::style::StyleCx;
use egui::text::{LayoutJob, TextWrapping};
use egui::{
    Color32, CornerRadius, FontFamily, FontId, Rect, Response, Role, Sense, Stroke, StrokeKind, TextFormat, TextureId, Ui, WidgetInfo, pos2, vec2,
};

use super::chip::{Chip, ChipStyle};
use super::corner;
use super::keycap::{Keycap, KeycapStyle};

/// The colors and sizes of a [`SceneTile`] and an [`AddTile`].
#[derive(Clone, Debug, PartialEq)]
pub struct TileStyle {
    /// In place of the picture before one is drawn.
    pub empty: Color32,
    pub border: Color32,
    /// The border of a selected tile or the preview's tile.
    pub selected: Color32,
    /// The border of the program's tile.
    pub program: Color32,
    /// Over the picture while the pointer is on the tile.
    pub hover: Color32,
    pub text: Color32,
    pub font: FontId,
    /// The height of the name's row under the picture.
    pub label_height: f32,
    /// The space between the picture's edges and the key and chips.
    pub inset: f32,
    pub radius: u8,
    /// Seconds the hover takes to fade in and out.
    pub fade: f32,
    pub focus: Stroke,
    pub key: KeycapStyle,
    pub on_air: ChipStyle,
    pub accent: ChipStyle,
}

impl TileStyle {
    /// The style from the tokens of `cx`, with the chips and the key in `semibold`. The fade is
    /// the theme's hover time.
    pub fn new(cx: &StyleCx<'_>, semibold: FontFamily) -> TileStyle {
        let t = &cx.tokens;
        TileStyle {
            empty: cx.color(t.color.surface.sunken).egui(),
            border: cx.color(t.color.line.border).egui(),
            selected: cx.color(t.color.accent.solid).egui(),
            program: cx.color(t.color.status.bad.solid).egui(),
            hover: cx.color(t.color.state.hover).egui(),
            text: cx.color(t.color.text.normal).egui(),
            font: FontId::proportional(t.typography.label.size),
            label_height: 20.0,
            inset: 4.0,
            radius: corner(t.shape.radius.sm),
            fade: t.motion.hover.millis as f32 / 1000.0,
            focus: cx.focus_look().stroke,
            key: KeycapStyle::new(cx, semibold.clone()),
            on_air: ChipStyle::on_air(cx, semibold.clone()),
            accent: ChipStyle::accent(cx, semibold),
        }
    }

    /// The tile's height for `width` and a picture `aspect` times as tall as it is wide. An
    /// aspect that is not a finite number from 0.1 to 4 reads as 9 / 16.
    pub fn height(&self, width: f32, aspect: f32) -> f32 {
        let aspect = if aspect.is_finite() && (0.1..=4.0).contains(&aspect) { aspect } else { 9.0 / 16.0 };
        width.max(0.0) * aspect + self.label_height
    }
}

/// What a tile shows of the scene's place in the outputs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TileMarks {
    pub selected: bool,
    pub program: bool,
    pub preview: bool,
}

/// One scene tile. Texts are shown as given, already translated.
pub struct SceneTile<'a> {
    name: &'a str,
    access: &'a str,
    picture: Option<TextureId>,
    shortcut: Option<&'a str>,
    marks: TileMarks,
    program: &'a str,
    preview: &'a str,
    style: &'a TileStyle,
}

impl<'a> SceneTile<'a> {
    /// A tile showing `name`, which a screen reader reads as `access`.
    pub fn new(name: &'a str, access: &'a str, style: &'a TileStyle) -> SceneTile<'a> {
        SceneTile { name, access, picture: None, shortcut: None, marks: TileMarks::default(), program: "", preview: "", style }
    }

    /// The scene's picture, drawn over the whole picture area.
    pub fn picture(mut self, picture: Option<TextureId>) -> Self {
        self.picture = picture;
        self
    }

    /// The shortcut that shows the scene, written as the platform writes it.
    pub fn shortcut(mut self, shortcut: Option<&'a str>) -> Self {
        self.shortcut = shortcut;
        self
    }

    /// The marks, and the texts of the program and preview chips.
    pub fn marks(mut self, marks: TileMarks, program: &'a str, preview: &'a str) -> Self {
        (self.marks, self.program, self.preview) = (marks, program, preview);
        self
    }

    /// Draw the tile in `rect`, taking clicks and the keyboard focus under `id`.
    pub fn show_at(self, ui: &mut Ui, rect: Rect, id: egui::Id) -> Response {
        let s = self.style;
        let r = ui.interact(rect, id, Sense::click());
        let access = self.access.to_owned();
        r.widget_info(|| WidgetInfo::labeled(Role::Button, true, &access));
        if !ui.is_rect_visible(rect) {
            return r;
        }
        let rtl = craft_ui::runtime::env::Env::of(ui).view().direction().is_rtl();
        let painter = ui.painter_at(rect.expand(s.focus.width * 2.0 + 2.0));
        let picture = Rect::from_min_max(rect.min, pos2(rect.max.x, (rect.max.y - s.label_height).max(rect.min.y)));
        let radius = CornerRadius::same(s.radius);
        painter.rect_filled(picture, radius, s.empty);
        if let Some(texture) = self.picture {
            painter.image(texture, picture, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
        }
        let lit = ui.ctx().animate_bool_with_time(id.with("hover"), r.hovered(), s.fade);
        if lit > 0.0 {
            painter.rect_filled(picture, radius, s.hover.gamma_multiply(lit));
        }
        let (width, color) = if self.marks.program {
            (2.0, s.program)
        } else if self.marks.selected || self.marks.preview {
            (2.0, s.selected)
        } else {
            (1.0, s.border)
        };
        painter.rect_stroke(picture, radius, Stroke::new(width, color), StrokeKind::Inside);
        // The chips from the end corner toward the start, program first.
        let mut end = if rtl { picture.left() + s.inset } else { picture.right() - s.inset };
        let top = picture.top() + s.inset;
        let mut chips_start = end;
        for (on, text, style) in [(self.marks.program, self.program, &s.on_air), (self.marks.preview, self.preview, &s.accent)] {
            if !on || text.is_empty() {
                continue;
            }
            let chip = Chip::new(text, style);
            let w = chip.width(&painter);
            // A chip that does not fit inside the picture is left out, so none is drawn cut off.
            let fits = if rtl { end + w <= picture.right() - s.inset } else { end - w >= picture.left() + s.inset };
            if !fits {
                break;
            }
            let x = if rtl { end } else { end - w };
            chip.paint(&painter, pos2(x, top));
            end = if rtl { end + w + s.inset } else { end - w - s.inset };
            chips_start = end;
        }
        // The key goes at the start corner, in the room the chips leave.
        if let Some(shortcut) = self.shortcut {
            let start = if rtl { picture.right() - s.inset } else { picture.left() + s.inset };
            let room = if rtl { start - chips_start } else { chips_start - start };
            let key = Keycap::new(shortcut, &s.key);
            let x = if rtl { start - key.width(&painter) } else { start };
            key.paint(&painter, pos2(x, top), room.max(0.0));
        }
        let label = Rect::from_min_max(pos2(rect.left(), picture.bottom()), rect.max);
        let mut job = LayoutJob::single_section(self.name.to_owned(), TextFormat::simple(s.font.clone(), s.text));
        job.wrap = TextWrapping::truncate_at_width(label.width().max(0.0));
        let g = painter.layout_job(job);
        let x = if rtl { label.right() - g.size().x } else { label.left() };
        painter.galley(pos2(x, label.center().y - g.size().y / 2.0), g, s.text);
        if r.has_focus() {
            painter.rect_stroke(picture.expand(2.0), radius, s.focus, StrokeKind::Outside);
        }
        r
    }
}

/// The tile that adds a scene: a plus over its label, which is also its name for a screen
/// reader. The text is shown as given, already translated.
pub struct AddTile<'a> {
    label: &'a str,
    style: &'a TileStyle,
}

impl<'a> AddTile<'a> {
    pub fn new(label: &'a str, style: &'a TileStyle) -> AddTile<'a> {
        AddTile { label, style }
    }

    /// Draw the tile in `rect`, taking clicks and the keyboard focus under `id`.
    pub fn show_at(self, ui: &mut Ui, rect: Rect, id: egui::Id) -> Response {
        let s = self.style;
        let r = ui.interact(rect, id, Sense::click());
        let label = self.label.to_owned();
        r.widget_info(|| WidgetInfo::labeled(Role::Button, true, &label));
        if !ui.is_rect_visible(rect) {
            return r;
        }
        let painter = ui.painter_at(rect.expand(s.focus.width * 2.0 + 2.0));
        let picture = Rect::from_min_max(rect.min, pos2(rect.max.x, (rect.max.y - s.label_height).max(rect.min.y)));
        let radius = CornerRadius::same(s.radius);
        let lit = ui.ctx().animate_bool_with_time(id.with("hover"), r.hovered(), s.fade);
        if lit > 0.0 {
            painter.rect_filled(picture, radius, s.hover.gamma_multiply(lit));
        }
        painter.rect_stroke(picture, radius, Stroke::new(1.0, s.border), StrokeKind::Inside);
        let arm = (picture.height().min(picture.width()) * 0.15).clamp(0.0, 12.0);
        let c = picture.center();
        let stroke = Stroke::new(1.5, s.text);
        painter.line_segment([c - vec2(arm, 0.0), c + vec2(arm, 0.0)], stroke);
        painter.line_segment([c - vec2(0.0, arm), c + vec2(0.0, arm)], stroke);
        let row = Rect::from_min_max(pos2(rect.left(), picture.bottom()), rect.max);
        let mut job = LayoutJob::single_section(self.label.to_owned(), TextFormat::simple(s.font.clone(), s.text));
        job.wrap = TextWrapping::truncate_at_width(row.width().max(0.0));
        let g = painter.layout_job(job);
        painter.galley(pos2(row.center().x - g.size().x / 2.0, row.center().y - g.size().y / 2.0), g, s.text);
        if r.has_focus() {
            painter.rect_stroke(picture.expand(2.0), radius, s.focus, StrokeKind::Outside);
        }
        r
    }
}

#[cfg(test)]
mod tests {
    use egui::{Event, Modifiers, PointerButton, RawInput};

    use super::*;

    fn style() -> TileStyle {
        StyleCx::with_root(&crate::test_context(), |cx| TileStyle::new(cx, FontFamily::Proportional))
    }

    const RECT: Rect = Rect { min: pos2(10.0, 10.0), max: pos2(106.0, 84.0) };

    fn raw(events: Vec<Event>) -> RawInput {
        RawInput { events, screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0))), ..RawInput::default() }
    }

    fn draw(ctx: &egui::Context, st: &TileStyle, events: Vec<Event>, tile: impl Fn(&TileStyle) -> SceneTile<'_>) -> (Response, egui::FullOutput) {
        draw_in(ctx, st, RECT, events, tile)
    }

    fn draw_in(
        ctx: &egui::Context,
        st: &TileStyle,
        rect: Rect,
        events: Vec<Event>,
        tile: impl Fn(&TileStyle) -> SceneTile<'_>,
    ) -> (Response, egui::FullOutput) {
        let mut resp = None;
        let mut out = ctx.run_ui(raw(events), |ui| resp = Some(tile(st).show_at(ui, rect, egui::Id::unique("tile"))));
        out.textures_delta.clear();
        let Some(resp) = resp else { panic!("the tile was not drawn") };
        (resp, out)
    }

    fn strokes(out: &egui::FullOutput) -> Vec<(f32, Color32)> {
        out.shapes
            .iter()
            .filter_map(
                |c| if let egui::Shape::Rect(s) = &c.shape { (s.stroke.width > 0.0).then_some((s.stroke.width, s.stroke.color)) } else { None },
            )
            .collect()
    }

    fn fills(out: &egui::FullOutput) -> Vec<Color32> {
        out.shapes.iter().filter_map(|c| if let egui::Shape::Rect(s) = &c.shape { Some(s.fill) } else { None }).collect()
    }

    #[test]
    fn the_program_tile_has_the_program_border_and_a_selected_tile_the_accent_border() {
        let st = style();
        let ctx = egui::Context::default();
        let marks = |selected, program, preview| TileMarks { selected, program, preview };
        let (_, out) = draw(&ctx, &st, vec![], |st| SceneTile::new("Talk", "Scene, Talk, 2", st).marks(marks(true, true, true), "PGM", "PVW"));
        assert!(strokes(&out).contains(&(2.0, st.program)), "{:?}", strokes(&out));
        let (_, out) = draw(&ctx, &st, vec![], |st| SceneTile::new("Talk", "Scene, Talk, 2", st).marks(marks(false, false, true), "PGM", "PVW"));
        assert!(strokes(&out).contains(&(2.0, st.selected)));
        let (_, out) = draw(&ctx, &st, vec![], |st| SceneTile::new("Talk", "Scene, Talk, 2", st).marks(marks(true, false, false), "PGM", "PVW"));
        assert!(strokes(&out).contains(&(2.0, st.selected)));
        let (_, out) = draw(&ctx, &st, vec![], |st| SceneTile::new("Talk", "Scene, Talk, 2", st));
        assert!(strokes(&out).contains(&(1.0, st.border)) && !strokes(&out).iter().any(|(w, _)| *w == 2.0));
    }

    #[test]
    fn the_chips_and_the_key_are_drawn_only_when_given() {
        let st = style();
        let ctx = egui::Context::default();
        let marks = TileMarks { selected: false, program: true, preview: true };
        let wide = Rect::from_min_size(pos2(10.0, 10.0), vec2(200.0, 132.0));
        let (_, out) =
            draw_in(&ctx, &st, wide, vec![], |st| SceneTile::new("Talk", "Scene, Talk, 2", st).shortcut(Some("Ctrl+2")).marks(marks, "PGM", "PVW"));
        let f = fills(&out);
        assert!(f.contains(&st.on_air.fill) && f.contains(&st.accent.fill) && f.contains(&st.key.fill), "{f:?}");
        let (_, out) = draw(&ctx, &st, vec![], |st| SceneTile::new("Talk", "Scene, Talk, 2", st));
        let f = fills(&out);
        assert!(!f.contains(&st.on_air.fill) && !f.contains(&st.accent.fill) && !f.contains(&st.key.fill), "{f:?}");
    }

    #[test]
    fn a_key_with_no_room_beside_the_chips_is_left_out_and_the_chips_stay() {
        let st = style();
        let ctx = egui::Context::default();
        let marks = TileMarks { selected: false, program: true, preview: true };
        let narrow = Rect::from_min_size(pos2(10.0, 10.0), vec2(72.0, 60.0));
        let (_, out) = draw_in(&ctx, &st, narrow, vec![], |st| {
            SceneTile::new("Talk", "Scene, Talk, 2", st).shortcut(Some("Ctrl+Shift+2")).marks(marks, "PGM", "PVW")
        });
        let f = fills(&out);
        assert!(f.contains(&st.on_air.fill) && !f.contains(&st.key.fill), "{f:?}");
    }

    #[test]
    fn a_chip_that_does_not_fit_inside_the_picture_is_left_out() {
        let st = style();
        let ctx = egui::Context::default();
        let marks = TileMarks { selected: false, program: true, preview: true };
        let tiny = Rect::from_min_size(pos2(10.0, 10.0), vec2(24.0, 40.0));
        let (_, out) = draw_in(&ctx, &st, tiny, vec![], |st| SceneTile::new("Talk", "Scene, Talk, 2", st).marks(marks, "PGM", "PVW"));
        let f = fills(&out);
        assert!(!f.contains(&st.on_air.fill) && !f.contains(&st.accent.fill), "{f:?}");
        assert!(strokes(&out).contains(&(2.0, st.program)), "the border still marks the program");
    }

    #[test]
    fn a_tile_shows_its_picture_and_the_empty_fill_before_one() {
        let st = style();
        let ctx = egui::Context::default();
        let texture = TextureId::User(7);
        let (_, out) = draw(&ctx, &st, vec![], |st| SceneTile::new("Talk", "Scene, Talk, 2", st).picture(Some(texture)));
        let images = out.shapes.iter().filter(|c| matches!(&c.shape, egui::Shape::Mesh(m) if m.texture_id == texture)).count();
        assert_eq!(images, 1);
        assert!(fills(&out).contains(&st.empty));
    }

    #[test]
    fn with_no_fade_the_hover_shows_in_full_on_its_first_frame() {
        let over = || vec![Event::PointerMoved(RECT.center())];
        let lit = |fade: f32| {
            let st = TileStyle { fade, ..style() };
            let ctx = egui::Context::default();
            draw(&ctx, &st, over(), |st| SceneTile::new("Talk", "Scene, Talk, 2", st));
            let (_, out) = draw(&ctx, &st, over(), |st| SceneTile::new("Talk", "Scene, Talk, 2", st));
            fills(&out).contains(&st.hover)
        };
        assert!(lit(0.0), "reduced motion: the hover fill is the full hover color at once");
        assert!(!lit(10.0), "with a fade, the hover is still on its way in");
    }

    #[test]
    fn a_tile_reports_its_click_and_a_screen_reader_hears_its_name() {
        let st = style();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let at = RECT.center();
        let b = |pressed| Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        fn tile(st: &TileStyle) -> SceneTile<'_> {
            SceneTile::new("Talk", "Scene, Talk, 2, in program", st)
        }
        let (r, _) = draw(&ctx, &st, vec![], tile);
        assert!(!r.clicked());
        let (r, out) = draw(&ctx, &st, vec![Event::PointerMoved(at), b(true), b(false)], tile);
        assert!(r.clicked());
        let Some(update) = out.platform_output.accesskit_update else { panic!("no accessibility tree") };
        assert!(update.nodes.iter().any(|(_, n)| n.role() == Role::Button && n.label() == Some("Scene, Talk, 2, in program")));
    }

    #[test]
    fn the_add_tile_is_a_button_named_by_its_label() {
        let st = style();
        let ctx = egui::Context::default();
        ctx.enable_accesskit();
        let mut out = ctx.run_ui(raw(vec![]), |ui| {
            AddTile::new("Add Scene", &st).show_at(ui, RECT, egui::Id::unique("add"));
        });
        out.textures_delta.clear();
        let Some(update) = out.platform_output.accesskit_update.take() else { panic!("no accessibility tree") };
        assert!(update.nodes.iter().any(|(_, n)| n.role() == Role::Button && n.label() == Some("Add Scene")));
    }

    #[test]
    fn a_tile_height_is_its_picture_and_its_label_row() {
        let st = style();
        assert_eq!(st.height(96.0, 9.0 / 16.0), 54.0 + st.label_height);
        assert_eq!(st.height(96.0, f32::NAN), 54.0 + st.label_height, "a broken aspect reads as 16:9");
        assert_eq!(st.height(96.0, 1000.0), 54.0 + st.label_height);
        assert_eq!(st.height(-5.0, 1.0), st.label_height);
    }
}
