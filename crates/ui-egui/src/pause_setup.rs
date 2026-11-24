//! The Pause setup popover that the Pause button's arrow opens. It sets up one pause: the
//! countdown's length, the countdown text with its size and position, a picture or video with its
//! fit and background, what happens when the countdown reaches zero, and whether the scene's audio
//! is muted. Its start button runs `pause.start` with every choice, and the choices apply to that
//! pause only; the profile's pause screen settings stay as they are.

use craft_keymap::CommandRef;
use egui::{Id, Key, Modifiers, RichText, Ui};
use serde_json::json;
use studiocraft_model::{FitMode, PauseAnchor, PauseAtZero, PauseSettings};

use crate::i18n::Msg;
use crate::kit::segmented::{Segmented, SegmentedStyle};

/// The countdown lengths the popover offers, in seconds; 0 is no countdown.
pub const DURATIONS: [u32; 6] = [60, 120, 300, 600, 900, 0];

/// The labels of [`DURATIONS`], in English.
pub const DURATION_LABELS: [&str; 6] = ["1 min", "2 min", "5 min", "10 min", "15 min", "None"];

/// The ways the picture or video can be sized to the canvas, with their labels in English.
pub const FITS: [(FitMode, &str); 3] = [(FitMode::Fit, "Fit"), (FitMode::Fill, "Fill"), (FitMode::Stretch, "Stretch")];

/// The name commands use for `fit`.
fn fit_id(fit: FitMode) -> &'static str {
    match fit {
        FitMode::Fit => "fit",
        FitMode::Fill => "fill",
        FitMode::Stretch => "stretch",
    }
}

/// `seconds` as the Length field shows it: `m:ss`, or `h:mm:ss` from an hour.
pub fn length_text(seconds: u32) -> String {
    studiocraft_studio::countdown_text(u64::from(seconds) * 1000)
}

/// The seconds a Length field's `text` stands for: a number of minutes (`10`), `m:ss` or
/// `h:mm:ss`, from 0 to 12 hours. `None` for any other text.
pub fn parse_length(text: &str) -> Option<u32> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    let mut total: u64 = 0;
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || p.len() > 5 || !p.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let v: u64 = p.parse().ok()?;
        if i > 0 && (p.len() != 2 || v >= 60) {
            return None;
        }
        total = total * 60 + v;
    }
    if parts.len() == 1 {
        total *= 60;
    }
    let seconds = u32::try_from(total).ok()?;
    PauseSettings::COUNTDOWN_S.contains(&seconds).then_some(seconds)
}

/// What keeps the start button disabled, as the field it belongs to shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The Length field does not read as a length from 0 to 12 hours.
    Length,
    /// The countdown text breaks a rule of [`PauseSettings::check_text`]; the message is in English.
    Text(String),
    /// The picture or video is not a file type the pause screen can show, or its path is too long.
    Media,
    /// The Background field does not read as a color.
    Background,
}

/// The choices of one pause, starting from the profile's pause screen settings.
#[derive(Clone, Debug, PartialEq)]
pub struct PauseSetup {
    /// The countdown's length in seconds; 0 for no countdown.
    pub countdown_s: u32,
    /// The Length field's text.
    pub length: String,
    pub size_pct: u32,
    pub anchor: PauseAnchor,
    /// The countdown text, with one `{time}`.
    pub text: String,
    pub zero_text: String,
    /// A picture or video file; empty for none.
    pub media: String,
    pub fit: FitMode,
    /// The color under the picture or video, `#rrggbb` or `#rrggbbaa`.
    pub background: String,
    pub at_zero: PauseAtZero,
    pub mute_scene: bool,
    /// The file extensions, in lowercase, of the pictures and videos the pause screen can show.
    extensions: Vec<String>,
    /// Which opening of the popover this is, so a file dialog answer meant for an earlier one is
    /// dropped.
    pub opening: u64,
    /// The keyboard focus moves into the popover on the next frame.
    focus: bool,
}

/// What the popover asks for on one frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Outcome {
    /// The `pause.start` command to run; the popover closes.
    pub start: Option<CommandRef>,
    /// Cancel or Esc: the popover closes and the Pause button takes the keyboard focus.
    pub cancel: bool,
    /// Choose…: ask for a picture or video file. The popover stays open.
    pub choose_media: bool,
    /// Pause Screen Settings…: the popover closes and Settings opens at the Pause screen section.
    pub settings: bool,
}

impl PauseSetup {
    /// The choices of the profile's pause screen settings `p`, with the keyboard focus on the
    /// selected duration on the next frame. `extensions` are the file types a picture or video
    /// may have, and `opening` counts the popover's openings.
    pub fn new(p: &PauseSettings, extensions: Vec<String>, opening: u64) -> PauseSetup {
        PauseSetup {
            countdown_s: p.countdown_s,
            length: length_text(p.countdown_s),
            size_pct: p.size_pct,
            anchor: p.anchor,
            text: p.text.clone(),
            zero_text: p.zero_text.clone(),
            media: p.media.clone(),
            fit: p.fit,
            background: p.background.clone(),
            at_zero: p.at_zero,
            mute_scene: p.mute_scene,
            extensions,
            opening,
            focus: true,
        }
    }

    /// Whether the picture or video is empty, or a path the pause screen can show by its type.
    pub fn media_ok(&self) -> bool {
        if self.media.is_empty() {
            return true;
        }
        let ext = std::path::Path::new(&self.media).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
        PauseSettings::check_media(&self.media).is_ok() && ext.is_some_and(|e| self.extensions.contains(&e))
    }

    /// The index in [`DURATIONS`] of the countdown's length, when it is one of them.
    pub fn duration(&self) -> Option<usize> {
        DURATIONS.iter().position(|&d| d == self.countdown_s)
    }

    /// Choose the duration at `index` in [`DURATIONS`]; an index past the end changes nothing.
    pub fn choose(&mut self, index: usize) {
        if let Some(&d) = DURATIONS.get(index) {
            self.countdown_s = d;
            self.length = length_text(d);
        }
    }

    /// Read the Length field's text into the countdown's length when it reads as one.
    pub fn set_length(&mut self, text: &str) {
        self.length = text.to_string();
        if let Some(s) = parse_length(text) {
            self.countdown_s = s;
        }
    }

    /// The first problem that keeps the pause from starting.
    pub fn problem(&self) -> Option<Problem> {
        if parse_length(&self.length).is_none() {
            return Some(Problem::Length);
        }
        if let Err(e) = PauseSettings::check_text(&self.text) {
            return Some(Problem::Text(e));
        }
        if !self.media_ok() {
            return Some(Problem::Media);
        }
        if studiocraft_model::settings::parse_color(&self.background).is_none() {
            return Some(Problem::Background);
        }
        None
    }

    /// The `pause.start` command with every choice, or `None` while a field has a problem.
    pub fn command(&self) -> Option<CommandRef> {
        if self.problem().is_some() {
            return None;
        }
        Some(CommandRef::with_params(
            "pause.start",
            json!({
                "media": self.media,
                "countdown_s": self.countdown_s,
                "text": self.text,
                "zero_text": self.zero_text,
                "at_zero": self.at_zero.id(),
                "anchor": self.anchor.id(),
                "size_pct": self.size_pct,
                "fit": fit_id(self.fit),
                "background": self.background,
                "mute_scene": self.mute_scene,
            }),
        ))
    }

    /// The start button's label: Pause for the countdown's length, or Pause Without a Countdown.
    pub fn start_label(&self) -> String {
        if self.countdown_s == 0 {
            tl!("Pause Without a Countdown").to_string()
        } else {
            craft_i18n::fmt(tl!("Pause for {time}"), &[("time", &length_text(self.countdown_s))])
        }
    }

    /// The id of the duration segment at `index`.
    #[cfg(test)]
    pub fn duration_id(index: usize) -> Id {
        Id::unique("pause-setup-durations").with(index)
    }

    /// The id of the Length field.
    pub fn length_id() -> Id {
        Id::unique("pause-setup-length")
    }

    /// Draw the popover's contents. The left and right arrow keys move between the durations
    /// while one has the keyboard focus, Enter starts the pause unless a button has the focus,
    /// and Esc cancels.
    pub fn show(&mut self, ui: &mut Ui, durations: &SegmentedStyle) -> Outcome {
        let mut out = Outcome::default();
        let mut buttons: Vec<Id> = Vec::new();
        ui.set_width(360.0);
        ui.strong(tl!("Pause the program"));
        ui.add_space(6.0);

        ui.label(tl!("Countdown length"));
        let labels: Vec<&str> = DURATION_LABELS.iter().map(|l| tl!(l)).collect();
        let selected = self.duration().unwrap_or(DURATIONS.len());
        let seg = Segmented::new(Id::unique("pause-setup-durations"), &labels, selected, durations).show(ui);
        if let Some(i) = seg.changed {
            self.choose(i);
        }
        // The selected duration takes the keyboard focus when the popover opens, or the Length
        // field when no duration matches.
        let focus = std::mem::take(&mut self.focus);
        if focus && let Some(r) = self.duration().and_then(|i| seg.segments.get(i)) {
            r.request_focus();
        }
        let focus_length = focus && self.duration().is_none();
        ui.add_space(6.0);

        egui::Grid::new("pause-setup-grid").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label(tl!("Length"));
            ui.vertical(|ui| {
                let mut length = self.length.clone();
                let r = ui.add(egui::TextEdit::singleline(&mut length).id(Self::length_id()).char_limit(12).desired_width(80.0));
                if focus_length {
                    r.request_focus();
                }
                if r.changed() {
                    self.set_length(&length);
                }
                if parse_length(&self.length).is_none() {
                    ui.colored_label(ui.visuals().error_fg_color, tl!("Type a length like 5:00, up to 12:00:00"));
                }
            });
            ui.end_row();
            ui.label(tl!("Size"));
            ui.add(egui::DragValue::new(&mut self.size_pct).range(PauseSettings::SIZE_PCT).suffix(format!(" {}", tl!("% of the canvas height"))));
            ui.end_row();
            ui.label(tl!("Position"));
            egui::Grid::new("pause-setup-anchor").num_columns(3).spacing([2.0, 2.0]).show(ui, |ui| {
                for (i, a) in PauseAnchor::ALL.into_iter().enumerate() {
                    let r = ui.selectable_label(self.anchor == a, tl!(crate::settings::anchor_label(a)));
                    buttons.push(r.id);
                    if r.clicked() {
                        self.anchor = a;
                    }
                    if i % 3 == 2 {
                        ui.end_row();
                    }
                }
            });
            ui.end_row();
            ui.label(tl!("Text"));
            ui.vertical(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.text).char_limit(PauseSettings::MAX_TEXT).desired_width(240.0));
                match PauseSettings::check_text(&self.text) {
                    Err(e) => {
                        ui.colored_label(ui.visuals().error_fg_color, Msg::plain(e.as_str()).shown());
                    }
                    Ok(()) => {
                        ui.label(RichText::new(tl!("{time} shows the time left")).small().weak());
                    }
                }
            });
            ui.end_row();
        });
        ui.separator();

        ui.label(tl!("Picture or video"));
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.media).hint_text(tl!("None")).char_limit(PauseSettings::MAX_MEDIA_PATH).desired_width(200.0));
            let r = ui.button(tl!("Choose…"));
            buttons.push(r.id);
            if r.clicked() {
                out.choose_media = true;
            }
            let r = ui.add_enabled(!self.media.is_empty(), egui::Button::new(tl!("No media")));
            buttons.push(r.id);
            if r.clicked() {
                self.media.clear();
            }
        });
        if self.media_ok() {
            ui.label(RichText::new(tl!("A video loops.")).small().weak());
        } else {
            ui.colored_label(ui.visuals().error_fg_color, tl!("Choose a picture or video file"));
        }
        egui::Grid::new("pause-setup-media").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label(tl!("Fit"));
            let fits: Vec<&str> = FITS.iter().map(|(_, l)| tl!(l)).collect();
            let at = FITS.iter().position(|(f, _)| *f == self.fit).unwrap_or(FITS.len());
            let r = Segmented::new(Id::unique("pause-setup-fit"), &fits, at, durations).show(ui);
            buttons.extend(r.segments.iter().map(|s| s.id));
            if let Some((f, _)) = r.changed.and_then(|i| FITS.get(i)) {
                self.fit = *f;
            }
            ui.end_row();
            ui.label(tl!("Background"));
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    let parsed = studiocraft_model::settings::parse_color(&self.background);
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(16.0, 16.0), egui::Sense::hover());
                    if let Some([r, g, b, a]) = parsed {
                        ui.painter().rect_filled(rect, 3.0, egui::Color32::from_rgba_unmultiplied(r, g, b, a));
                    }
                    ui.painter().rect_stroke(rect, 3.0, ui.visuals().widgets.noninteractive.bg_stroke, egui::StrokeKind::Inside);
                    ui.add(egui::TextEdit::singleline(&mut self.background).char_limit(9).desired_width(90.0));
                });
                if studiocraft_model::settings::parse_color(&self.background).is_none() {
                    ui.colored_label(ui.visuals().error_fg_color, tl!("Type a color like #101014"));
                }
            });
            ui.end_row();
        });
        ui.separator();

        ui.label(tl!("When the countdown reaches zero"));
        let keep = craft_i18n::fmt(tl!("Keep the screen and show \"{text}\""), &[("text", &self.zero_text)]);
        for (z, label) in [(PauseAtZero::Keep, keep), (PauseAtZero::Resume, tl!("Resume the program").to_string())] {
            let r = ui.radio_value(&mut self.at_zero, z, label);
            buttons.push(r.id);
        }
        let r = ui.checkbox(&mut self.mute_scene, tl!("Mute the scene's audio while paused"));
        buttons.push(r.id);
        ui.label(RichText::new(tl!("The stream and the recording keep running and show the pause screen.")).small().weak());
        ui.separator();

        let command = self.command();
        let mut start = false;
        ui.horizontal(|ui| {
            let r = ui.button(tl!("Pause Screen Settings…"));
            buttons.push(r.id);
            if r.clicked() {
                out.settings = true;
            }
            ui.with_layout(crate::i18n::trailing(ui), |ui| {
                let r = ui.add_enabled(command.is_some(), egui::Button::new(self.start_label()));
                buttons.push(r.id);
                start |= r.clicked();
                let r = ui.button(tl!("Cancel"));
                buttons.push(r.id);
                if r.clicked() {
                    out.cancel = true;
                }
            });
        });

        // Enter on a button clicks that button; elsewhere in the popover it starts the pause.
        let focused = ui.memory(|m| m.focused());
        if !focused.is_some_and(|f| buttons.contains(&f)) && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Enter)) {
            start = true;
        }
        if ui.input(|i| i.key_pressed(Key::Escape)) {
            out.cancel = true;
        }
        if start && !out.cancel {
            out.start = command;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use egui::{Event, RawInput, Rect, pos2, vec2};

    use super::*;

    fn defaults() -> PauseSettings {
        PauseSettings::default()
    }

    /// The popover for `p`, taking PNG pictures and MP4 videos.
    fn open(p: &PauseSettings) -> PauseSetup {
        PauseSetup::new(p, vec!["mp4".into(), "png".into()], 1)
    }

    #[test]
    fn lengths_read_as_minutes_or_clock_text_up_to_twelve_hours() {
        assert_eq!(parse_length("5"), Some(300), "a bare number is minutes");
        assert_eq!(parse_length(" 5:00 "), Some(300));
        assert_eq!(parse_length("0:45"), Some(45));
        assert_eq!(parse_length("1:02:03"), Some(3723));
        assert_eq!(parse_length("12:00:00"), Some(43_200));
        assert_eq!(parse_length("0"), Some(0));
        for bad in ["", ":", "5:", "5:7", "5:60", "1:60:00", "12:00:01", "721", "1:2:3:4", "-1", "5.5", "99999999", "1:00:000", "５:00"] {
            assert_eq!(parse_length(bad), None, "{bad:?}");
        }
        assert_eq!(length_text(300), "5:00");
        assert_eq!(length_text(3723), "1:02:03");
        assert_eq!(parse_length(&length_text(43_200)), Some(43_200), "the field's own text reads back");
    }

    #[test]
    fn the_setup_starts_from_the_profile_and_sends_every_choice() {
        let setup = open(&defaults());
        assert_eq!((setup.countdown_s, setup.length.as_str(), setup.duration()), (300, "5:00", Some(2)));
        let Some(cmd) = setup.command() else { panic!("the profile's settings start a pause") };
        assert_eq!(cmd.id, "pause.start");
        assert_eq!(
            cmd.params,
            json!({
                "media": "",
                "countdown_s": 300,
                "text": "Back in {time}",
                "zero_text": "Back in a moment",
                "at_zero": "keep",
                "anchor": "bottom_center",
                "size_pct": defaults().size_pct,
                "fit": "fit",
                "background": "#101014ff",
                "mute_scene": defaults().mute_scene,
            })
        );
        let mut s = setup.clone();
        s.choose(5);
        assert_eq!((s.countdown_s, s.length.as_str(), s.start_label()), (0, "0:00", "Pause Without a Countdown".to_string()));
        s.choose(99);
        assert_eq!(s.countdown_s, 0, "an index past the end changes nothing");
        s.set_length("3:30");
        assert_eq!((s.countdown_s, s.duration(), s.start_label()), (210, None, "Pause for 3:30".to_string()));
        s.fit = FitMode::Stretch;
        s.anchor = PauseAnchor::TopRight;
        s.at_zero = PauseAtZero::Resume;
        let Some(cmd) = s.command() else { panic!("no command") };
        assert_eq!(
            (cmd.params["fit"].as_str(), cmd.params["anchor"].as_str(), cmd.params["at_zero"].as_str()),
            (Some("stretch"), Some("top_right"), Some("resume"))
        );
    }

    #[test]
    fn a_field_with_a_problem_keeps_the_pause_from_starting() {
        let mut s = open(&defaults());
        s.set_length("5:7");
        assert_eq!((s.problem(), s.countdown_s), (Some(Problem::Length), 300), "the last good length stays");
        assert_eq!(s.command(), None);
        s.set_length("2:00");
        s.text = "Back soon".into();
        assert!(matches!(s.problem(), Some(Problem::Text(_))), "no {{time}}");
        s.text = "{time} {time}".into();
        assert!(matches!(s.problem(), Some(Problem::Text(_))), "{{time}} twice");
        s.text = "Back in {time}".into();
        s.background = "blue".into();
        assert_eq!(s.problem(), Some(Problem::Background));
        s.background = "#000000".into();
        assert_eq!(s.problem(), None);
        assert!(s.command().is_some());
        for bad in ["notes.doc", "clip", "song.mp3", "C:/pictures/"] {
            s.media = bad.into();
            assert_eq!((s.problem(), s.command()), (Some(Problem::Media), None), "{bad:?}");
        }
        s.media = format!("{}.png", "a".repeat(PauseSettings::MAX_MEDIA_PATH));
        assert_eq!(s.problem(), Some(Problem::Media), "a path past the byte limit");
        s.media = "with\0nul.png".into();
        assert_eq!(s.problem(), Some(Problem::Media));
        for good in ["C:/pictures/Back.PNG", "/clips/loop.mp4", ""] {
            s.media = good.into();
            assert_eq!(s.problem(), None, "{good:?}");
        }
    }

    /// Draw the popover in a frame with `events`, after a first frame that draws it.
    fn run(ctx: &egui::Context, setup: &mut PauseSetup, events: Vec<Event>) -> Outcome {
        let st = crate::theme::with_root(ctx, SegmentedStyle::new);
        let mut out = Outcome::default();
        let raw = RawInput { events, screen_rect: Some(Rect::from_min_size(pos2(0.0, 0.0), vec2(800.0, 800.0))), ..RawInput::default() };
        let mut full = ctx.run_ui(raw, |ui| {
            out = setup.show(ui, &st);
        });
        full.textures_delta.clear();
        out
    }

    fn key(k: Key) -> Vec<Event> {
        vec![
            Event::Key { key: k, physical_key: Some(k), pressed: true, repeat: false, modifiers: Modifiers::NONE },
            Event::Key { key: k, physical_key: Some(k), pressed: false, repeat: false, modifiers: Modifiers::NONE },
        ]
    }

    #[test]
    fn the_keyboard_moves_between_durations_starts_and_cancels() {
        let ctx = crate::test_context();
        let mut s = open(&defaults());
        run(&ctx, &mut s, vec![]);
        assert_eq!(ctx.memory(|m| m.focused()), Some(PauseSetup::duration_id(2)), "the selected duration takes the focus");
        run(&ctx, &mut s, key(Key::ArrowRight));
        assert_eq!((s.countdown_s, s.length.as_str()), (600, "10:00"), "the right arrow picks 10 min");
        run(&ctx, &mut s, key(Key::ArrowLeft));
        run(&ctx, &mut s, key(Key::ArrowLeft));
        assert_eq!(s.countdown_s, 120);
        let out = run(&ctx, &mut s, key(Key::Enter));
        let Some(cmd) = out.start else { panic!("Enter did not start the pause: {out:?}") };
        assert_eq!(cmd.params["countdown_s"], json!(120));
        let out = run(&ctx, &mut s, key(Key::Escape));
        assert!(out.cancel && out.start.is_none(), "{out:?}");
    }

    #[test]
    fn enter_does_not_start_a_pause_with_a_problem_and_a_custom_length_focuses_the_field() {
        let ctx = crate::test_context();
        let mut p = defaults();
        p.countdown_s = 90;
        let mut s = open(&p);
        run(&ctx, &mut s, vec![]);
        assert_eq!(ctx.memory(|m| m.focused()), Some(PauseSetup::length_id()), "no duration chip matches 1:30");
        s.text = "Back soon".into();
        let out = run(&ctx, &mut s, key(Key::Enter));
        assert_eq!(out.start, None, "the text has no {{time}}");
    }
}
