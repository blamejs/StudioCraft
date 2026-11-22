//! The settings of a Staged windows source: the windows and displays staged in its slots and the
//! slot it presents. The source (`studiocraft-sources`) captures them, and the engine's
//! `present.*` commands change the presented slot.

use crate::Settings;

/// The plug-in id of the Staged windows source.
pub const KIND: &str = "staged_windows";

/// How many windows or displays one source stages.
pub const SLOTS: usize = 6;

/// The setting key of each slot, in order. A slot holds a window key, a display key
/// ([`display_key`]), or `""` when it is empty.
pub const SLOT_KEYS: [&str; SLOTS] = ["window1", "window2", "window3", "window4", "window5", "window6"];

/// The setting that holds the presented slot, 1 to [`SLOTS`].
pub const PRESENTED: &str = "presented";

/// The first character of a slot value that holds a display. The rest of the value is the
/// display's id, in the form the Display capture source stores in its `display` setting. A value
/// that does not start with it is a window key.
pub const DISPLAY_MARK: char = '\u{1e}';

/// The slot value that stages the display with `id`.
pub fn display_key(id: &str) -> String {
    format!("{DISPLAY_MARK}{id}")
}

/// The display id in the slot value `key`, or `None` when the value holds a window.
pub fn display_id(key: &str) -> Option<&str> {
    key.strip_prefix(DISPLAY_MARK)
}

/// The window or display staged in slot `n` (1 is the first), or `None` when the slot is empty or
/// there is no such slot.
pub fn staged(s: &Settings, n: usize) -> Option<&str> {
    let key = SLOT_KEYS.get(n.checked_sub(1)?)?;
    s.str(key).filter(|w| !w.is_empty())
}

/// The presented slot, 1 to [`SLOTS`]. A missing or out-of-range value reads as 1.
pub fn presented(s: &Settings) -> usize {
    s.i64(PRESENTED).and_then(|n| usize::try_from(n).ok()).filter(|n| (1..=SLOTS).contains(n)).unwrap_or(1)
}

/// The next slot that holds a window or display after the presented one (`forward`), or before
/// it, wrapping around. It is the presented slot itself when that is the only slot that is not
/// empty, and `None` when every slot is empty.
pub fn step(s: &Settings, forward: bool) -> Option<usize> {
    let from = presented(s);
    (1..=SLOTS)
        .map(|k| if forward { (from - 1 + k) % SLOTS + 1 } else { (from - 1 + SLOTS - k % SLOTS) % SLOTS + 1 })
        .find(|&n| staged(s, n).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slots(filled: &[usize], presented: i64) -> Settings {
        let mut s = Settings::new().with(PRESENTED, presented);
        for n in filled {
            s.set(SLOT_KEYS[n - 1], format!("app.exe\u{1f}Window {n}"));
        }
        s
    }

    #[test]
    fn a_slot_holds_a_window_only_when_its_key_is_not_empty() {
        let s = slots(&[2], 1).with("window3", "");
        assert_eq!(staged(&s, 1), None);
        assert_eq!(staged(&s, 2), Some("app.exe\u{1f}Window 2"));
        assert_eq!(staged(&s, 3), None);
        assert_eq!(staged(&s, 0), None);
        assert_eq!(staged(&s, SLOTS + 1), None);
    }

    #[test]
    fn a_display_key_is_the_mark_and_the_display_id_and_a_window_key_is_not_one() {
        assert_eq!(display_key("65537"), "\u{1e}65537");
        assert_eq!(display_id(&display_key("65537")), Some("65537"));
        for window in ["app.exe\u{1f}Window 1", "A title only", "\u{1f}Untitled program", ""] {
            assert_eq!(display_id(window), None, "{window:?}");
        }
        let s = slots(&[], 1).with("window4", display_key("2"));
        assert_eq!(staged(&s, 4), Some("\u{1e}2"));
        assert_eq!(step(&s, true), Some(4), "a display counts as a filled slot");
    }

    #[test]
    fn an_out_of_range_presented_slot_reads_as_the_first() {
        assert_eq!(presented(&slots(&[], 4)), 4);
        for bad in [0, -1, 7, i64::MAX, i64::MIN] {
            assert_eq!(presented(&slots(&[], bad)), 1, "{bad}");
        }
        assert_eq!(presented(&Settings::new().with(PRESENTED, "2")), 1);
    }

    #[test]
    fn stepping_skips_empty_slots_and_wraps_around() {
        let s = slots(&[2, 5], 2);
        assert_eq!(step(&s, true), Some(5));
        assert_eq!(step(&s, false), Some(5));
        let s = slots(&[2, 5], 5);
        assert_eq!(step(&s, true), Some(2));
        assert_eq!(step(&s, false), Some(2));
        let s = slots(&[1, 3, 6], 3);
        assert_eq!(step(&s, true), Some(6));
        assert_eq!(step(&s, false), Some(1));
        // From an empty presented slot, the nearest filled slot on that side.
        let s = slots(&[1, 6], 4);
        assert_eq!(step(&s, true), Some(6));
        assert_eq!(step(&s, false), Some(1));
    }

    #[test]
    fn stepping_stays_on_a_lone_window_and_finds_nothing_in_empty_slots() {
        assert_eq!(step(&slots(&[3], 3), true), Some(3));
        assert_eq!(step(&slots(&[3], 3), false), Some(3));
        assert_eq!(step(&slots(&[], 1), true), None);
        assert_eq!(step(&slots(&[], 1), false), None);
    }
}
