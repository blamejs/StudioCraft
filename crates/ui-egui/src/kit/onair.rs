//! The Go Live, Record and Pause buttons: a glyph, a label and, while the output runs, its timer.
//!
//! An idle button has the control fill and a 1-point outline. A running one is filled with the
//! live or recording red and draws its glyph, label and timer in the on-accent color; a running
//! Pause or Resume button is filled with the warning amber and draws them in the on-warning color.
//! While the output starts or stops, the button has the pressed fill and a turning arc in place of
//! the glyph; a stopping output cannot be clicked. A compact button leaves the label out and keeps
//! the glyph and, while running, the timer.
//!
//! A split button is a face joined to a [`MenuArrow`] at its end side: the face's end corners are
//! square, and the arrow opens a menu.

use egui::{Color32, CornerRadius, FontId, Pos2, Rect, Response, Role, Sense, Shape, Stroke, StrokeKind, TextStyle, Ui, WidgetInfo, pos2, vec2};

use super::tokens::Palette;

/// Which output a button is for; it picks the glyph and the running fill.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glyph {
    /// A circle, filled while recording.
    Record,
    /// A dot with two waves on each side.
    Live,
    /// Two upright bars.
    Pause,
    /// A triangle pointing right, in every writing direction.
    Resume,
}

/// What the output is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnAirState {
    Idle,
    Starting,
    Running,
    Stopping,
    /// The output cannot start now.
    Disabled,
}

/// The colors and sizes of an [`OnAirButton`].
#[derive(Clone, Debug, PartialEq)]
pub struct OnAirStyle {
    pub fill: Color32,
    pub hover: Color32,
    pub pressed: Color32,
    pub outline: Color32,
    pub text: Color32,
    pub text_disabled: Color32,
    pub on_fill: Color32,
    /// The running fill of a [`Glyph::Live`] button.
    pub live: Color32,
    /// The running fill of a [`Glyph::Record`] button.
    pub recording: Color32,
    /// The running fill of a [`Glyph::Pause`] or [`Glyph::Resume`] button.
    pub warn: Color32,
    /// The glyph, label and timer on the [`warn`](Self::warn) fill.
    pub on_warn: Color32,
    pub height: f32,
    /// The space between the edge and the content on each side.
    pub padding: f32,
    /// The glyph's width and height.
    pub glyph: f32,
    /// The space between the glyph, the label and the timer.
    pub gap: f32,
    pub radius: u8,
    /// The timer's font; the label uses the button text style.
    pub timer_font: FontId,
}

impl OnAirStyle {
    /// The style from a palette, with the button `height` tall and corners of `radius`.
    pub fn new(p: &Palette, height: f32, radius: u8, timer_font: FontId) -> OnAirStyle {
        OnAirStyle {
            fill: p.control,
            hover: p.control_hover,
            pressed: p.control_active,
            outline: p.line_strong,
            text: p.text,
            text_disabled: p.text_disabled,
            on_fill: p.on_accent,
            live: p.on_air,
            recording: p.recording,
            warn: p.warn,
            on_warn: p.on_warn,
            height,
            padding: 12.0,
            glyph: 12.0,
            gap: 6.0,
            radius,
            timer_font,
        }
    }
}

/// One output button. Texts are shown as given, already translated.
pub struct OnAirButton<'a> {
    glyph: Glyph,
    state: OnAirState,
    label: &'a str,
    timer: Option<&'a str>,
    compact: bool,
    joined: bool,
    name: &'a str,
    style: &'a OnAirStyle,
}

/// The fill and the glyph and text color of a running button with `glyph`.
fn running_colors(s: &OnAirStyle, glyph: Glyph) -> (Color32, Color32) {
    match glyph {
        Glyph::Live => (s.live, s.on_fill),
        Glyph::Record => (s.recording, s.on_fill),
        Glyph::Pause | Glyph::Resume => (s.warn, s.on_warn),
    }
}

/// The button's fill, its glyph and text color, and its outline.
fn colors(s: &OnAirStyle, glyph: Glyph, state: OnAirState, pressed: bool, hovered: bool) -> (Color32, Color32, Option<Color32>) {
    let (running_fill, on_running) = running_colors(s, glyph);
    match state {
        OnAirState::Running if pressed => (running_fill.gamma_multiply(0.85), on_running, None),
        OnAirState::Running => (running_fill, on_running, None),
        OnAirState::Starting | OnAirState::Stopping => (s.pressed, s.text, Some(s.outline)),
        OnAirState::Disabled => (s.fill, s.text_disabled, Some(s.outline)),
        OnAirState::Idle if pressed => (s.pressed, s.text, Some(s.outline)),
        OnAirState::Idle if hovered => (s.hover, s.text, Some(s.outline)),
        OnAirState::Idle => (s.fill, s.text, Some(s.outline)),
    }
}

/// Corners of `radius`, square on the end side when `end` and on the start side when `start`.
fn corners(ui: &Ui, radius: u8, start: bool, end: bool) -> CornerRadius {
    let (left, right) = if craft_ui::direction::is_rtl(ui) { (end, start) } else { (start, end) };
    let side = |square: bool| if square { 0 } else { radius };
    CornerRadius { nw: side(left), sw: side(left), ne: side(right), se: side(right) }
}

impl<'a> OnAirButton<'a> {
    /// A button showing `label`; `name` is what a screen reader announces, such as
    /// "Recording, 00:12:34".
    pub fn new(glyph: Glyph, state: OnAirState, label: &'a str, name: &'a str, style: &'a OnAirStyle) -> OnAirButton<'a> {
        OnAirButton { glyph, state, label, timer: None, compact: false, joined: false, name, style }
    }

    /// Square the end corners, for the face of a split button whose [`MenuArrow`] follows.
    pub fn joined(mut self, joined: bool) -> OnAirButton<'a> {
        self.joined = joined;
        self
    }

    /// The timer, shown after the label while the output runs.
    pub fn timer(mut self, timer: Option<&'a str>) -> OnAirButton<'a> {
        self.timer = timer.filter(|_| self.state == OnAirState::Running);
        self
    }

    /// Leave the label out.
    pub fn compact(mut self, compact: bool) -> OnAirButton<'a> {
        self.compact = compact;
        self
    }

    fn text_width(ui: &Ui, text: &str, font: FontId) -> f32 {
        ui.painter().layout_no_wrap(text.to_owned(), font, Color32::WHITE).size().x.ceil()
    }

    /// The width the button takes: the padding, the glyph, and the label and timer it shows.
    pub fn width(&self, ui: &Ui) -> f32 {
        let s = self.style;
        let mut w = 2.0 * s.padding + s.glyph;
        if !self.compact && !self.label.is_empty() {
            w += s.gap + Self::text_width(ui, self.label, TextStyle::Button.resolve(ui.style()));
        }
        if let Some(t) = self.timer {
            w += s.gap + Self::text_width(ui, t, s.timer_font.clone());
        }
        w.max(s.height)
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let s = self.style;
        let width = self.width(ui);
        let clickable = !matches!(self.state, OnAirState::Stopping | OnAirState::Disabled);
        let (rect, r) = ui.allocate_exact_size(vec2(width, s.height), if clickable { Sense::click() } else { Sense::hover() });
        r.widget_info(|| WidgetInfo::labeled(Role::Button, clickable, self.name));
        let pressed = clickable && r.is_pointer_button_down_on();
        let (fill, fg, outline) = colors(s, self.glyph, self.state, pressed, r.hovered());
        let radius = corners(ui, s.radius, false, self.joined);
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, radius, fill);
        if let Some(line) = outline {
            painter.rect_stroke(rect, radius, Stroke::new(1.0, line), StrokeKind::Inside);
        }
        let mut x = rect.left() + s.padding;
        let glyph = Rect::from_min_size(pos2(x, rect.center().y - s.glyph / 2.0), vec2(s.glyph, s.glyph));
        match self.state {
            OnAirState::Starting | OnAirState::Stopping => {
                let t = ui.input(|i| i.time) as f32;
                ui.ctx().request_repaint();
                painter.add(Shape::line(arc(glyph.center(), s.glyph * 0.42, t * 6.0, t * 6.0 + 4.4, 16), Stroke::new(1.5, fg)));
            }
            _ => paint_glyph(&painter, self.glyph, glyph, fg, self.state == OnAirState::Running),
        }
        x = glyph.right();
        if !self.compact && !self.label.is_empty() {
            x += s.gap;
            let g = painter.layout_no_wrap(self.label.to_owned(), TextStyle::Button.resolve(ui.style()), fg);
            let w = g.size().x;
            painter.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, fg);
            x += w.ceil();
        }
        if let Some(t) = self.timer {
            x += s.gap;
            let g = painter.layout_no_wrap(t.to_owned(), s.timer_font.clone(), fg);
            painter.galley(pos2(x, rect.center().y - g.size().y / 2.0), g, fg);
        }
        r
    }
}

/// The arrow of a split button: a downward chevron in the face's fill, with square start corners
/// and a divider on its start side. A click opens its menu; texts are shown as given.
pub struct MenuArrow<'a> {
    glyph: Glyph,
    state: OnAirState,
    name: &'a str,
    style: &'a OnAirStyle,
}

impl<'a> MenuArrow<'a> {
    /// The arrow after a face with `glyph` in `state`; `name` is what a screen reader announces.
    pub fn new(glyph: Glyph, state: OnAirState, name: &'a str, style: &'a OnAirStyle) -> MenuArrow<'a> {
        MenuArrow { glyph, state, name, style }
    }

    /// The width the arrow takes: the glyph and the padding on its end side.
    pub fn width(&self) -> f32 {
        self.style.glyph + self.style.padding
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let s = self.style;
        let clickable = !matches!(self.state, OnAirState::Stopping | OnAirState::Disabled);
        let (rect, r) = ui.allocate_exact_size(vec2(self.width(), s.height), if clickable { Sense::click() } else { Sense::hover() });
        r.widget_info(|| WidgetInfo::labeled(Role::Button, clickable, self.name));
        let pressed = clickable && r.is_pointer_button_down_on();
        let (fill, fg, outline) = colors(s, self.glyph, self.state, pressed, r.hovered());
        let painter = ui.painter_at(rect);
        let radius = corners(ui, s.radius, true, false);
        painter.rect_filled(rect, radius, fill);
        if let Some(line) = outline {
            painter.rect_stroke(rect, radius, Stroke::new(1.0, line), StrokeKind::Inside);
        }
        let start = if craft_ui::direction::is_rtl(ui) { rect.right() - 0.5 } else { rect.left() + 0.5 };
        let divider = outline.unwrap_or_else(|| fg.gamma_multiply(0.4));
        painter.line_segment([pos2(start, rect.top() + 5.0), pos2(start, rect.bottom() - 5.0)], Stroke::new(1.0, divider));
        let c = rect.center();
        let w = s.glyph * 0.3;
        painter.add(Shape::line(vec![pos2(c.x - w, c.y - w * 0.5), pos2(c.x, c.y + w * 0.5), pos2(c.x + w, c.y - w * 0.5)], Stroke::new(1.5, fg)));
        r
    }
}

fn arc(c: Pos2, r: f32, from: f32, to: f32, n: usize) -> Vec<Pos2> {
    (0..=n).map(|i| from + (to - from) * i as f32 / n as f32).map(|a| pos2(c.x + r * a.cos(), c.y + r * a.sin())).collect()
}

/// Draw `glyph` in `rect`; a record circle is filled while `on`.
pub fn paint_glyph(painter: &egui::Painter, glyph: Glyph, rect: Rect, color: Color32, on: bool) {
    let c = rect.center();
    let r = rect.width().min(rect.height()) / 2.0;
    match glyph {
        Glyph::Record if on => {
            painter.circle_filled(c, r * 0.8, color);
        }
        Glyph::Record => {
            painter.circle_stroke(c, r * 0.8 - 0.75, Stroke::new(1.5, color));
        }
        Glyph::Live => {
            painter.circle_filled(c, r * 0.28, color);
            let stroke = Stroke::new(1.5, color);
            for (radius, spread) in [(r * 0.62, 0.75), (r * 1.0, 0.8)] {
                painter.add(Shape::line(arc(c, radius, -spread, spread, 8), stroke));
                painter.add(Shape::line(arc(c, radius, std::f32::consts::PI - spread, std::f32::consts::PI + spread, 8), stroke));
            }
        }
        Glyph::Pause => {
            let (w, h) = (r * 0.5, r * 1.6);
            for x in [c.x - r * 0.55, c.x + r * 0.05] {
                painter.rect_filled(Rect::from_min_size(pos2(x, c.y - h / 2.0), vec2(w, h)), 1.0, color);
            }
        }
        Glyph::Resume => {
            let h = r * 0.8;
            let tip = pos2(c.x + h, c.y);
            painter.add(Shape::convex_polygon(vec![pos2(c.x - h * 0.7, c.y - h), tip, pos2(c.x - h * 0.7, c.y + h)], color, Stroke::NONE));
        }
    }
}

#[cfg(test)]
mod tests {
    use egui::{Event, Modifiers, PointerButton, RawInput};

    use super::*;

    fn style() -> OnAirStyle {
        OnAirStyle::new(&Palette::DARK, 28.0, 4, FontId::monospace(12.0))
    }

    fn raw(events: Vec<Event>) -> RawInput {
        RawInput { events, screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(600.0, 100.0))), ..RawInput::default() }
    }

    /// One frame with one button; its response and the painted shapes.
    fn draw(ctx: &egui::Context, events: Vec<Event>, make: impl Fn(&OnAirStyle) -> OnAirButton<'_>) -> (Response, Vec<egui::epaint::ClippedShape>) {
        let st = style();
        let mut resp = None;
        let mut out = ctx.run_ui(raw(events), |ui| resp = Some(make(&st).show(ui)));
        out.textures_delta.clear();
        let Some(resp) = resp else { panic!("the button was not drawn") };
        (resp, out.shapes)
    }

    fn filled(shapes: &[egui::epaint::ClippedShape], fill: Color32, rect: Rect) -> bool {
        shapes.iter().any(|c| matches!(&c.shape, egui::Shape::Rect(s) if s.fill == fill && s.rect == rect))
    }

    #[test]
    fn buttons_are_28_tall_and_as_wide_as_their_content() {
        let ctx = egui::Context::default();
        let (idle, _) = draw(&ctx, vec![], |s| OnAirButton::new(Glyph::Record, OnAirState::Idle, "Record", "Record", s));
        assert_eq!(idle.rect.height(), 28.0);
        let (long, _) = draw(&ctx, vec![], |s| OnAirButton::new(Glyph::Live, OnAirState::Idle, "Go Live now please", "Go Live", s));
        assert!(long.rect.width() > idle.rect.width() + 40.0, "{:?} {:?}", long.rect, idle.rect);
        let (compact, _) = draw(&ctx, vec![], |s| OnAirButton::new(Glyph::Record, OnAirState::Idle, "Record", "Record", s).compact(true));
        assert_eq!(compact.rect.width(), 36.0, "the padding and the glyph");
        let (timed, _) = draw(&ctx, vec![], |s| {
            OnAirButton::new(Glyph::Record, OnAirState::Running, "REC", "Recording, 00:00:03", s).timer(Some("00:00:03")).compact(true)
        });
        assert!(timed.rect.width() > 36.0 + 40.0, "a running compact button keeps its timer: {:?}", timed.rect);
        let (idle_timer, _) =
            draw(&ctx, vec![], |s| OnAirButton::new(Glyph::Record, OnAirState::Idle, "Record", "Record", s).timer(Some("00:00:03")).compact(true));
        assert_eq!(idle_timer.rect.width(), 36.0, "an idle button shows no timer");
    }

    #[test]
    fn a_running_output_fills_the_button_with_its_red() {
        let ctx = egui::Context::default();
        let p = Palette::DARK;
        let (r, shapes) =
            draw(&ctx, vec![], |s| OnAirButton::new(Glyph::Live, OnAirState::Running, "LIVE", "Live, 00:01:00", s).timer(Some("00:01:00")));
        assert!(filled(&shapes, p.on_air, r.rect));
        let (r, shapes) = draw(&ctx, vec![], |s| OnAirButton::new(Glyph::Record, OnAirState::Running, "REC", "Recording", s));
        assert!(filled(&shapes, p.recording, r.rect));
        let (r, shapes) = draw(&ctx, vec![], |s| OnAirButton::new(Glyph::Record, OnAirState::Idle, "Record", "Record", s));
        assert!(filled(&shapes, p.control, r.rect));
        assert!(!filled(&shapes, p.recording, r.rect));
    }

    #[test]
    fn a_paused_button_is_amber_with_dark_text_and_an_idle_one_is_a_control() {
        let ctx = egui::Context::default();
        let p = Palette::DARK;
        let (r, shapes) =
            draw(&ctx, vec![], |s| OnAirButton::new(Glyph::Resume, OnAirState::Running, "Resume", "Resume, 4:32 left", s).timer(Some("4:32")));
        assert!(filled(&shapes, p.warn, r.rect));
        assert!(!filled(&shapes, p.on_air, r.rect), "a pause does not read as on air");
        assert_eq!(colors(&style(), Glyph::Resume, OnAirState::Running, false, false).1, p.on_warn);
        let (r, shapes) = draw(&ctx, vec![], |s| OnAirButton::new(Glyph::Pause, OnAirState::Idle, "Pause", "Pause", s));
        assert!(filled(&shapes, p.control, r.rect));
    }

    #[test]
    fn a_split_face_squares_its_end_corners_and_its_arrow_its_start_corners() {
        let ctx = egui::Context::default();
        let mut shapes = Vec::new();
        let st = style();
        let mut out = ctx.run_ui(raw(vec![]), |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                OnAirButton::new(Glyph::Pause, OnAirState::Idle, "Pause", "Pause", &st).joined(true).show(ui);
                MenuArrow::new(Glyph::Pause, OnAirState::Idle, "Pause options", &st).show(ui);
            });
        });
        out.textures_delta.clear();
        shapes.extend(out.shapes);
        let radii: Vec<CornerRadius> = shapes
            .iter()
            .filter_map(|c| match &c.shape {
                egui::Shape::Rect(s) if s.fill == Palette::DARK.control => Some(s.corner_radius),
                _ => None,
            })
            .collect();
        assert_eq!(radii, [CornerRadius { nw: 4, sw: 4, ne: 0, se: 0 }, CornerRadius { nw: 0, sw: 0, ne: 4, se: 4 }]);
    }

    #[test]
    fn the_menu_arrow_is_a_named_button_that_clicks() {
        let ctx = egui::Context::default();
        let st = style();
        let mut resp = None;
        let mut out = ctx.run_ui(raw(vec![]), |ui| resp = Some(MenuArrow::new(Glyph::Pause, OnAirState::Idle, "Pause options", &st).show(ui)));
        out.textures_delta.clear();
        let Some(r) = resp else { panic!("the arrow was not drawn") };
        assert_eq!(r.rect.size(), vec2(24.0, 28.0));
        let mut resp = None;
        let mut out = ctx.run_ui(raw(click(r.rect.center())), |ui| {
            resp = Some(MenuArrow::new(Glyph::Pause, OnAirState::Idle, "Pause options", &st).show(ui));
        });
        out.textures_delta.clear();
        assert!(resp.is_some_and(|r| r.clicked()));
        let mut resp = None;
        let mut out = ctx.run_ui(raw(click(r.rect.center())), |ui| {
            resp = Some(MenuArrow::new(Glyph::Pause, OnAirState::Disabled, "Pause options", &st).show(ui));
        });
        out.textures_delta.clear();
        assert!(resp.is_some_and(|r| !r.clicked()), "a disabled arrow does not open its menu");
    }

    fn click(at: Pos2) -> Vec<Event> {
        let b = |pressed| Event::PointerButton { pos: at, button: PointerButton::Primary, pressed, modifiers: Modifiers::NONE };
        vec![Event::PointerMoved(at), b(true), b(false)]
    }

    #[test]
    fn a_stopping_or_disabled_button_cannot_be_clicked_and_a_starting_one_can() {
        for (state, clicks) in [
            (OnAirState::Idle, true),
            (OnAirState::Starting, true),
            (OnAirState::Running, true),
            (OnAirState::Stopping, false),
            (OnAirState::Disabled, false),
        ] {
            let ctx = egui::Context::default();
            let (r, _) = draw(&ctx, vec![], |s| OnAirButton::new(Glyph::Live, state, "Go Live", "Go Live", s));
            let (r, _) = draw(&ctx, click(r.rect.center()), |s| OnAirButton::new(Glyph::Live, state, "Go Live", "Go Live", s));
            assert_eq!(r.clicked(), clicks, "{state:?}");
        }
    }

    #[test]
    fn screen_readers_hear_the_name_not_the_label() {
        let ctx = egui::Context::default();
        let st = style();
        let button = || OnAirButton::new(Glyph::Record, OnAirState::Running, "REC", "Recording, 00:12:34", &st).timer(Some("00:12:34"));
        let mut id = None;
        let mut out = ctx.run_ui(raw(vec![]), |ui| id = Some(button().show(ui).id));
        out.textures_delta.clear();
        let Some(id) = id else { panic!("the button was not drawn") };
        // Focus moves to the button during the frame, as Tab moves it.
        let mut out = ctx.run_ui(raw(vec![]), |ui| {
            ui.memory_mut(|m| m.request_focus(id));
            button().show(ui);
        });
        out.textures_delta.clear();
        let names: Vec<Option<String>> = out
            .platform_output
            .events
            .iter()
            .filter_map(|e| match e {
                egui::output::OutputEvent::FocusGained(info) => Some(info.label.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(names, [Some("Recording, 00:12:34".to_owned())]);
    }
}
