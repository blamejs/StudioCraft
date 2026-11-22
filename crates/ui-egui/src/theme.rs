//! The window's look: the palettes, sizes and type scale of [`crate::kit::tokens`] applied to
//! egui's dark and light styles, and the keyboard focus ring.
//!
//! The window follows the system's light or dark setting; each one gets its own palette.

use egui::{Color32, CornerRadius, Event, FontId, Id, Key, Margin, Rect, Shadow, Stroke, StrokeKind, Style, TextStyle, Theme};

use crate::kit::tokens::{Palette, Sizes, TypeScale};

/// The named text style for field labels and menu shortcuts.
pub const LABEL: &str = "Label";
/// The named text style for dock and section titles.
pub const TITLE: &str = "Title";
/// The named text style for the live and recording timers.
pub const TIMER: &str = "Timer";

/// How long hover and open animations take, in seconds.
pub const ANIMATION_TIME: f32 = 0.12;

/// Shown before a checked menu item and the selected radio item.
pub const CHECK_MARK: &str = "\u{2713}";
pub const RADIO_MARK: &str = "\u{25cf}";

fn sizes_id() -> Id {
    Id::unique("studiocraft-theme-sizes")
}

fn focus_id() -> Id {
    Id::unique("studiocraft-focus-ring")
}

/// Apply the compact theme to both of `ctx`'s styles.
pub fn install(ctx: &egui::Context) {
    install_with(ctx, &Sizes::COMPACT);
}

/// Apply the theme with `sizes` to both of `ctx`'s styles.
pub fn install_with(ctx: &egui::Context, sizes: &Sizes) {
    ctx.style_mut_of(Theme::Dark, |s| apply(s, &Palette::DARK, sizes, &TypeScale::DEFAULT));
    ctx.style_mut_of(Theme::Light, |s| apply(s, &Palette::LIGHT, sizes, &TypeScale::DEFAULT));
    let sizes = *sizes;
    ctx.data_mut(|d| d.insert_temp(sizes_id(), sizes));
}

/// The palette of the theme `ctx` shows now.
pub fn palette(ctx: &egui::Context) -> &'static Palette {
    match ctx.theme() {
        Theme::Dark => &Palette::DARK,
        Theme::Light => &Palette::LIGHT,
    }
}

/// The sizes [`install_with`] applied; the compact set before it ran.
pub fn sizes(ctx: &egui::Context) -> Sizes {
    ctx.data(|d| d.get_temp::<Sizes>(sizes_id())).unwrap_or(Sizes::COMPACT)
}

/// Set `style`'s colors, spacing, text styles and animation time from the tokens.
pub fn apply(style: &mut Style, p: &Palette, s: &Sizes, t: &TypeScale) {
    let v = &mut style.visuals;
    v.dark_mode = p.dark;
    // port: P2 (theme and fonts on the framework) deletes this; each state's fills, radius and text
    // stroke, as the removed `craft_ui::theme::WidgetPalette::apply_to` set them.
    let w = &mut v.widgets;
    for (state, fill) in [
        (&mut w.noninteractive, p.surface),
        (&mut w.inactive, p.control),
        (&mut w.hovered, p.control_hover),
        (&mut w.active, p.control_active),
        (&mut w.open, p.control_active),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.corner_radius = CornerRadius::same(s.radius_control);
        state.fg_stroke = Stroke::new(1.0, p.text);
    }
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text_muted);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.line);
    for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active, &mut v.widgets.open] {
        w.bg_stroke = Stroke::new(1.0, p.line_strong);
    }
    v.selection.bg_fill = p.selection;
    v.selection.stroke = Stroke::new(1.0, p.accent_text);
    v.hyperlink_color = p.accent_text;
    v.warn_fg_color = p.warn;
    v.error_fg_color = p.on_air_text;
    v.weak_text_color = Some(p.text_hint);
    v.panel_fill = p.surface;
    v.window_fill = p.raised;
    v.extreme_bg_color = p.control;
    v.code_bg_color = p.control;
    v.faint_bg_color = p.background;
    v.window_stroke = Stroke::new(1.0, p.line);
    v.window_corner_radius = CornerRadius::same(s.radius_window);
    v.menu_corner_radius = CornerRadius::same(s.radius_window);
    let shadow = Color32::from_black_alpha(p.shadow_alpha);
    v.window_shadow = Shadow { offset: [0, 4], blur: 16, spread: 0, color: shadow };
    v.popup_shadow = Shadow { offset: [0, 4], blur: 8, spread: 0, color: shadow };

    let sp = &mut style.spacing;
    sp.item_spacing = s.item_spacing;
    sp.button_padding = s.button_padding;
    sp.interact_size = s.interact_size;
    sp.menu_margin = Margin::same(s.margin_menu);
    sp.window_margin = Margin::same(s.margin_window);
    sp.icon_width = s.icon_width;
    sp.icon_width_inner = s.icon_width_inner;
    sp.icon_spacing = s.icon_spacing;

    let ts = &mut style.text_styles;
    ts.insert(TextStyle::Body, FontId::proportional(t.body));
    ts.insert(TextStyle::Button, FontId::proportional(t.button));
    ts.insert(TextStyle::Small, FontId::proportional(t.small));
    ts.insert(TextStyle::Heading, FontId::new(t.heading, crate::fonts::semibold()));
    ts.insert(TextStyle::Monospace, FontId::monospace(t.monospace));
    ts.insert(TextStyle::Name(LABEL.into()), FontId::proportional(t.label));
    ts.insert(TextStyle::Name(TITLE.into()), FontId::new(t.title, crate::fonts::semibold()));
    ts.insert(TextStyle::Name(TIMER.into()), FontId::monospace(t.timer));

    style.animation_time = ANIMATION_TIME;
}

/// Whether the focus ring shows after `events`, given whether it showed before: a key that
/// moves focus (Tab, an arrow or F6) turns it on, a pointer press turns it off.
pub fn keyboard_focus_after(events: &[Event], before: bool) -> bool {
    let mut on = before;
    for e in events {
        match e {
            Event::Key { key: Key::Tab | Key::ArrowUp | Key::ArrowDown | Key::ArrowLeft | Key::ArrowRight | Key::F6, pressed: true, .. } => on = true,
            Event::PointerButton { pressed: true, .. } => on = false,
            _ => {}
        }
    }
    on
}

/// The rectangle the focus ring is stroked outside of: the widget grown by the gap.
pub fn ring_rect(widget: Rect, sizes: &Sizes) -> Rect {
    widget.expand(sizes.focus_gap)
}

/// Paint the ring around the widget with keyboard focus, when focus last moved by keyboard.
/// Call once per frame, after every panel and window.
pub fn paint_focus_ring(ctx: &egui::Context) {
    let before = ctx.data(|d| d.get_temp::<bool>(focus_id())).unwrap_or(false);
    let on = ctx.input(|i| keyboard_focus_after(&i.events, before));
    ctx.data_mut(|d| d.insert_temp(focus_id(), on));
    if !on {
        return;
    }
    let Some(focused) = ctx.memory(|m| m.focused()) else {
        return;
    };
    let Some(response) = ctx.read_response(focused) else {
        return;
    };
    // A widget scrolled out of sight has no visible part to mark.
    if !response.interact_rect.is_positive() {
        return;
    }
    let sizes = sizes(ctx);
    let reach = sizes.focus_gap + sizes.focus_width;
    // The ring goes on the widget's own layer, so a window or popup above the widget covers it.
    let painter = ctx.layer_painter(response.layer_id).with_clip_rect(response.interact_rect.expand(reach));
    let radius = CornerRadius::same(sizes.radius_control.saturating_add(sizes.focus_gap as u8));
    painter.rect_stroke(ring_rect(response.rect, &sizes), radius, Stroke::new(sizes.focus_width, palette(ctx).focus), StrokeKind::Outside);
}

#[cfg(test)]
mod tests {
    use egui::{Modifiers, PointerButton, RawInput, pos2, vec2};

    use super::*;

    fn installed() -> egui::Context {
        let ctx = egui::Context::default();
        install(&ctx);
        ctx
    }

    #[test]
    fn both_styles_take_the_compact_tokens() {
        let ctx = installed();
        for (theme, p) in [(Theme::Dark, Palette::DARK), (Theme::Light, Palette::LIGHT)] {
            let s = ctx.style_of(theme);
            assert_eq!(s.spacing.interact_size, vec2(24.0, 24.0));
            assert_eq!(s.spacing.button_padding, vec2(8.0, 4.0));
            assert_eq!(s.spacing.item_spacing, vec2(6.0, 4.0));
            assert_eq!(s.spacing.menu_margin, Margin::same(4));
            assert_eq!(s.text_styles[&TextStyle::Body].size, 12.0);
            assert_eq!(s.text_styles[&TextStyle::Button].size, 12.0);
            assert_eq!(s.text_styles[&TextStyle::Small].size, 11.0);
            assert_eq!(s.text_styles[&TextStyle::Heading].size, 13.0);
            assert_eq!(s.text_styles[&TextStyle::Heading].family, crate::fonts::semibold());
            assert_eq!(s.text_styles[&TextStyle::Name(TITLE.into())].family, crate::fonts::semibold());
            assert_eq!(s.text_styles[&TextStyle::Name(TIMER.into())].family, egui::FontFamily::Monospace);
            assert_eq!(s.animation_time, ANIMATION_TIME);
            assert_eq!(s.visuals.dark_mode, p.dark);
            assert_eq!(s.visuals.panel_fill, p.surface);
            assert_eq!(s.visuals.window_fill, p.raised);
            assert_eq!(s.visuals.widgets.inactive.bg_fill, p.control);
            assert_eq!(s.visuals.widgets.inactive.bg_stroke, Stroke::new(1.0, p.line_strong));
            assert_eq!(s.visuals.widgets.noninteractive.bg_stroke.color, p.line);
            assert_eq!(s.visuals.selection.stroke.color, p.accent_text);
        }
        assert_eq!(sizes(&ctx), Sizes::COMPACT);
    }

    #[test]
    fn comfortable_sizes_reach_the_styles() {
        let ctx = egui::Context::default();
        install_with(&ctx, &Sizes::COMFORTABLE);
        assert_eq!(ctx.style_of(Theme::Dark).spacing.interact_size, vec2(28.0, 28.0));
        assert_eq!(sizes(&ctx), Sizes::COMFORTABLE);
    }

    fn screen() -> RawInput {
        RawInput { screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(400.0, 300.0))), ..RawInput::default() }
    }

    #[test]
    fn a_short_button_is_24_tall_and_fits_its_label() {
        let ctx = installed();
        ctx.set_theme(Theme::Dark);
        let mut rect = Rect::NOTHING;
        let mut out = ctx.run_ui(screen(), |ui| rect = ui.button("Add").rect);
        out.textures_delta.clear();
        assert_eq!(rect.height(), 24.0);
        assert!(rect.width() < 60.0, "{rect:?}");
    }

    fn key(key: Key) -> Event {
        Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers: Modifiers::NONE }
    }

    fn press() -> Event {
        Event::PointerButton { pos: pos2(300.0, 250.0), button: PointerButton::Primary, pressed: true, modifiers: Modifiers::NONE }
    }

    #[test]
    fn the_ring_follows_keyboard_focus_moves_only() {
        assert!(!keyboard_focus_after(&[], false));
        assert!(keyboard_focus_after(&[key(Key::Tab)], false));
        assert!(keyboard_focus_after(&[key(Key::F6)], false));
        assert!(keyboard_focus_after(&[], true), "the ring stays until something turns it off");
        assert!(!keyboard_focus_after(&[press()], true));
        assert!(keyboard_focus_after(&[press(), key(Key::ArrowDown)], false), "the last event decides");
        assert!(!keyboard_focus_after(&[key(Key::A)], false), "a key that does not move focus");
        let release = Event::Key { key: Key::Tab, physical_key: Some(Key::Tab), pressed: false, repeat: false, modifiers: Modifiers::NONE };
        assert!(!keyboard_focus_after(&[release], false));
    }

    /// The focus-colored 2 px strokes painted in a frame with `events`.
    fn rings(ctx: &egui::Context, events: Vec<Event>) -> Vec<Rect> {
        let raw = RawInput { events, ..screen() };
        let mut out = ctx.run_ui(raw, |ui| {
            let r = ui.button("Add");
            r.request_focus();
            paint_focus_ring(ui.ctx());
        });
        out.textures_delta.clear();
        let focus = palette(ctx).focus;
        out.shapes
            .iter()
            .filter_map(|c| match &c.shape {
                egui::Shape::Rect(r) if r.stroke == Stroke::new(2.0, focus) => Some(r.rect),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_ring_is_painted_two_points_outside_a_focused_widget_after_tab() {
        let ctx = installed();
        ctx.set_theme(Theme::Dark);
        assert!(rings(&ctx, vec![]).is_empty(), "no ring before any key");
        let shown = rings(&ctx, vec![key(Key::Tab)]);
        assert_eq!(shown.len(), 1, "{shown:?}");
        let button = ctx.read_response(ctx.memory(|m| m.focused()).unwrap()).unwrap().rect;
        assert_eq!(shown[0], ring_rect(button, &Sizes::COMPACT));
        assert_eq!(shown[0], button.expand(2.0));
        assert_eq!(rings(&ctx, vec![]).len(), 1, "the ring stays while focus does not move by pointer");
        assert!(rings(&ctx, vec![press()]).is_empty(), "a pointer press hides it");
    }

    #[test]
    fn a_window_over_the_focused_widget_covers_its_ring() {
        let ctx = installed();
        ctx.set_theme(Theme::Dark);
        let p = palette(&ctx);
        let mut button = Rect::NOTHING;
        let mut shapes = Vec::new();
        // The window is laid out invisibly on its first frame and then fades in; a second per
        // frame ends the fade.
        for (t, events) in [vec![key(Key::Tab)], vec![], vec![], vec![]].into_iter().enumerate() {
            let raw = RawInput { events, time: Some(t as f64), predicted_dt: 1.0, ..screen() };
            let mut out = ctx.run_ui(raw, |ui| {
                egui::CentralPanel::default().show(ui, |ui| {
                    let r = ui.button("Add");
                    r.request_focus();
                    button = r.interact_rect;
                });
                egui::Window::new("Over")
                    .title_bar(false)
                    .resizable(false)
                    .fixed_pos(pos2(0.0, 0.0))
                    .fixed_size(vec2(200.0, 150.0))
                    .show(ui.ctx(), |ui| ui.label("Settings"));
                paint_focus_ring(ui.ctx());
            });
            out.textures_delta.clear();
            shapes = out.shapes;
        }
        let ring = Stroke::new(2.0, p.focus);
        let rings: Vec<usize> =
            shapes.iter().enumerate().filter(|(_, c)| matches!(&c.shape, egui::Shape::Rect(r) if r.stroke == ring)).map(|(i, _)| i).collect();
        assert_eq!(rings.len(), 1, "the ring is still painted under the window");
        assert_eq!(shapes[rings[0]].clip_rect, button.expand(Sizes::COMPACT.focus_gap + Sizes::COMPACT.focus_width));
        // A frame with a shadow paints as a `Shape::Vec` of the shadow and the filled rect.
        fn covers(shape: &egui::Shape, fill: Color32, button: Rect) -> bool {
            match shape {
                egui::Shape::Rect(r) => r.fill == fill && r.rect.contains_rect(button),
                egui::Shape::Vec(v) => v.iter().any(|s| covers(s, fill, button)),
                _ => false,
            }
        }
        let window = shapes.iter().position(|c| covers(&c.shape, p.raised, button));
        let Some(window) = window else { panic!("the window covering the button was not painted") };
        assert!(rings[0] < window, "the ring is drawn before the window that covers it (ring {}, window {window})", rings[0]);
    }
}
