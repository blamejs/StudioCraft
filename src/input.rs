//! Synthetic input for the window: mouse, key and text events, queued in batches of one frame
//! each. The self-test and the control channel's `ui.*` methods both build their events here.

use std::collections::VecDeque;

use craft_keymap::{Mods, Platform, Trigger};
use egui::{Event, Modifiers, PointerButton, Pos2};

/// Most frames of input waiting at once.
pub const MAX_QUEUED_FRAMES: usize = 10_000;

/// Events waiting for the next frames, one batch per frame.
#[derive(Default)]
pub struct EventQueue {
    frames: VecDeque<Vec<Event>>,
}

impl EventQueue {
    /// Queue `frames` after the ones already waiting. Nothing is queued when they would make the
    /// queue longer than [`MAX_QUEUED_FRAMES`].
    pub fn push(&mut self, frames: Vec<Vec<Event>>) -> Result<(), String> {
        if self.frames.len().saturating_add(frames.len()) > MAX_QUEUED_FRAMES {
            return Err(format!("at most {MAX_QUEUED_FRAMES} frames of input can wait; try again once the window has caught up"));
        }
        self.frames.extend(frames);
        Ok(())
    }

    /// Add the next frame's events.
    pub fn inject(&mut self, raw: &mut egui::RawInput) {
        if let Some(ev) = self.frames.pop_front() {
            raw.events.extend(ev);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

/// egui's modifiers for a chord's: the command modifier is Cmd on macOS and Ctrl elsewhere.
pub fn modifiers(m: Mods) -> Modifiers {
    if Platform::current().is_mac() {
        Modifiers { alt: m.alt, ctrl: m.ctrl, shift: m.shift, mac_cmd: m.cmd, command: m.cmd }
    } else {
        let cmd = m.cmd || m.ctrl;
        Modifiers { alt: m.alt, ctrl: cmd, shift: m.shift, mac_cmd: false, command: cmd }
    }
}

/// Move to `pos`, then press and release `button` `count` times, one press per frame.
pub fn click(pos: Pos2, button: PointerButton, count: u32, modifiers: Modifiers) -> Vec<Vec<Event>> {
    let mut frames = vec![vec![Event::PointerMoved(pos)]];
    for _ in 0..count {
        frames.push(vec![
            Event::PointerButton { pos, button, pressed: true, modifiers },
            Event::PointerButton { pos, button, pressed: false, modifiers },
        ]);
    }
    frames
}

/// Press the primary button at `from`, move to `to` in `steps` equal moves, one per frame, and
/// release it there.
pub fn drag(from: Pos2, to: Pos2, steps: u32, modifiers: Modifiers) -> Vec<Vec<Event>> {
    let button = PointerButton::Primary;
    let mut frames = vec![vec![Event::PointerMoved(from)], vec![Event::PointerButton { pos: from, button, pressed: true, modifiers }]];
    let n = steps.max(1);
    for k in 1..=n {
        let f = k as f32 / n as f32;
        frames.push(vec![Event::PointerMoved(from + (to - from) * f)]);
    }
    frames.push(vec![Event::PointerButton { pos: to, button, pressed: false, modifiers }]);
    frames
}

/// Press and release the chord of `trigger`, twice in two frames for a double press.
pub fn key(trigger: Trigger) -> Vec<Vec<Event>> {
    let key = trigger.chord.key.to_egui();
    let modifiers = modifiers(trigger.chord.mods);
    let press = vec![
        Event::Key { key, physical_key: Some(key), pressed: true, repeat: false, modifiers },
        Event::Key { key, physical_key: Some(key), pressed: false, repeat: false, modifiers },
    ];
    if trigger.double { vec![press.clone(), press] } else { vec![press] }
}

/// Type `text` into the focused field.
pub fn text(text: &str) -> Vec<Vec<Event>> {
    vec![vec![Event::Text(text.to_string())]]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_moves_in_equal_steps_and_releases_at_the_end() {
        let frames = drag(Pos2::new(10.0, 20.0), Pos2::new(110.0, 70.0), 4, Modifiers::NONE);
        assert_eq!(frames.len(), 7);
        assert_eq!(frames[3], vec![Event::PointerMoved(Pos2::new(60.0, 45.0))]);
        assert!(matches!(frames[6][0], Event::PointerButton { pressed: false, pos, .. } if pos == Pos2::new(110.0, 70.0)));
    }

    #[test]
    fn the_queue_refuses_more_than_it_holds() {
        let mut q = EventQueue::default();
        assert!(q.push(vec![Vec::new(); MAX_QUEUED_FRAMES]).is_ok());
        assert!(q.push(vec![Vec::new()]).is_err());
        let mut raw = egui::RawInput::default();
        q.inject(&mut raw);
        assert!(q.push(vec![Vec::new()]).is_ok());
    }
}
