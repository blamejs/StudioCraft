//! The banner above the program view while the program is paused: what viewers see and the time
//! left, with +1 min and Resume.
//!
//! The banner is a polite live region. It announces when the pause starts and when the countdown
//! reaches zero, and for a few seconds after Resume an announcement in its place says the program
//! resumed. The time left that the banner shows is not announced.

use egui::Ui;
use serde_json::json;

use crate::kit::banner::{Banner, BannerAction, BannerStyle};
use crate::{Act, StudioApp, theme};

/// How long the resume announcement stays in the accessibility tree, in seconds.
const RESUMED_FOR: f64 = 10.0;

/// The space between the banner and the views under it.
pub const GAP: f32 = 4.0;

/// What the banner showed last.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    /// The banner was shown on the last frame.
    shown: bool,
    /// When the program resumed after the banner was shown, in the window's input time.
    resumed_at: Option<f64>,
    /// Where the banner's buttons were drawn last, in order; empty while it is hidden.
    pub buttons: Vec<egui::Rect>,
}

impl State {
    /// One frame's step: whether the program is `paused` at input time `now`. Returns the
    /// resume announcement while it should stay.
    pub fn step(&mut self, paused: bool, now: f64) -> Option<&'static str> {
        if paused {
            self.shown = true;
            self.resumed_at = None;
            return None;
        }
        self.buttons.clear();
        if std::mem::take(&mut self.shown) {
            self.resumed_at = Some(now);
        }
        match self.resumed_at {
            Some(at) if now - at < RESUMED_FOR => Some(tl!("The program resumed")),
            Some(_) => {
                self.resumed_at = None;
                None
            }
            None => None,
        }
    }
}

/// The banner's text and its announcement while the program is paused. `paused` holds the
/// countdown's time left in milliseconds (`None` without a countdown), and `for_ms` how long the
/// program has been paused.
pub fn text(paused: Option<Option<u64>>, for_ms: Option<u64>) -> Option<(String, &'static str)> {
    let left = paused?;
    let started = tl!("Paused. Viewers see the pause screen.");
    Some(match left {
        Some(0) => {
            let ended = tl!("Paused. Viewers see the pause screen. The countdown ended.");
            (ended.to_string(), ended)
        }
        Some(ms) => {
            let time = studiocraft_studio::countdown_text(ms);
            (craft_i18n::fmt(tl!("Paused. Viewers see the pause screen. Back in {time}."), &[("time", &time)]), started)
        }
        None => {
            let time = studiocraft_studio::paused_text(for_ms.unwrap_or(0));
            (craft_i18n::fmt(tl!("Paused for {time}. Viewers see the pause screen."), &[("time", &time)]), started)
        }
    })
}

/// The banner's buttons, in the order they run: +1 min adds a minute to the countdown and Resume
/// ends the pause.
pub fn actions() -> [(BannerAction<'static>, Act); 2] {
    [
        (
            BannerAction { label: tl!("+1 min"), name: tid!("pause.extend", "Add a Minute to the Pause") },
            Act::run("pause.extend", json!({"seconds": 60})),
        ),
        (BannerAction { label: tl!("Resume"), name: tid!("pause.stop", "Resume Program") }, Act::run("pause.stop", json!({}))),
    ]
}

impl StudioApp {
    /// The banner at the top of `ui` while the program is paused; the height it took, with the gap
    /// under it.
    pub(crate) fn show_pause_banner(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) -> f32 {
        let paused = self.studio.paused();
        let now = ui.input(|i| i.time);
        let Some((shown, announce)) = text(paused, self.studio.paused_for_ms()) else {
            if let Some(resumed) = self.pause_banner.step(false, now) {
                let id = egui::Id::unique("pause-banner-resumed");
                ui.ctx().accesskit_node_builder(id, |node| {
                    node.set_role(egui::Role::Status);
                    node.set_live(egui::accesskit::Live::Polite);
                    node.set_label(resumed);
                });
            }
            return 0.0;
        };
        self.pause_banner.step(true, now);
        let ctx = ui.ctx().clone();
        let style = BannerStyle::new(theme::palette(&ctx), &theme::sizes(&ctx), egui::TextStyle::Body.resolve(ui.style()));
        let mut buttons = actions().map(Some);
        let labels: Vec<BannerAction> = buttons.iter().flatten().map(|(b, _)| *b).collect();
        let r = Banner::new(&shown, announce, &labels, &style).show(ui);
        self.pause_banner.buttons = r.actions.iter().map(|a| a.rect).collect();
        if let Some(i) = r.clicked()
            && let Some((_, act)) = buttons.get_mut(i).and_then(Option::take)
        {
            acts.push(act);
        }
        // The window draws again within a quarter second, so the time the banner shows stays current.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        style.height + GAP
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_banner_says_what_viewers_see_and_the_time() {
        assert_eq!(text(None, None), None, "nothing while not paused");
        let (shown, announce) = text(Some(Some(272_000)), Some(28_000)).unwrap_or_default();
        assert_eq!(shown, "Paused. Viewers see the pause screen. Back in 4:32.");
        assert_eq!(announce, "Paused. Viewers see the pause screen.");
        let (shown, announce) = text(Some(None), Some(130_400)).unwrap_or_default();
        assert_eq!(shown, "Paused for 2:10. Viewers see the pause screen.");
        assert_eq!(announce, "Paused. Viewers see the pause screen.", "the time paused is not announced");
        let (shown, announce) = text(Some(Some(0)), Some(300_000)).unwrap_or_default();
        assert_eq!(shown, "Paused. Viewers see the pause screen. The countdown ended.");
        assert_eq!(announce, shown, "zero is announced");
    }

    #[test]
    fn the_announcement_changes_only_at_the_start_at_zero_and_at_resume() {
        let announced = |left| text(Some(left), Some(0)).map(|(_, a)| a);
        assert_eq!(announced(Some(272_000)), announced(Some(271_000)), "a new second is not announced");
        assert_ne!(announced(Some(1_000)), announced(Some(0)));
        let mut s = State::default();
        assert_eq!(s.step(false, 0.0), None, "nothing before a pause");
        assert_eq!(s.step(true, 1.0), None);
        assert_eq!(s.step(false, 2.0), Some("The program resumed"));
        assert_eq!(s.step(false, 2.0 + RESUMED_FOR - 0.1), Some("The program resumed"));
        assert_eq!(s.step(false, 2.0 + RESUMED_FOR), None, "the announcement goes after a while");
        assert_eq!(s.step(false, 100.0), None);
        assert_eq!(s.step(true, 101.0), None, "a new pause clears it");
    }

    #[test]
    fn the_buttons_add_a_minute_and_resume() {
        let [(more, extend), (resume, stop)] = actions();
        assert_eq!((more.label, more.name), ("+1 min", "Add a Minute to the Pause"));
        assert_eq!((resume.label, resume.name), ("Resume", "Resume Program"));
        assert!(matches!(extend, Act::Run(c) if c.id == "pause.extend" && c.params == json!({"seconds": 60})));
        assert!(matches!(stop, Act::Run(c) if c.id == "pause.stop"));
    }
}
