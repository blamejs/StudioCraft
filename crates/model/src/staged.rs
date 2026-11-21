//! The settings of a Staged windows source: the windows staged in its slots and the slot it
//! presents. The source (`studiocraft-sources`) captures the windows, and the engine's
//! `present.*` commands change the presented slot.

use crate::Settings;

/// The plug-in id of the Staged windows source.
pub const KIND: &str = "staged_windows";

/// How many windows one source stages.
pub const SLOTS: usize = 6;

/// The setting key of each slot, in order. A slot holds a window key, or `""` when it is empty.
pub const SLOT_KEYS: [&str; SLOTS] = ["window1", "window2", "window3", "window4", "window5", "window6"];

/// The setting that holds the presented slot, 1 to [`SLOTS`].
pub const PRESENTED: &str = "presented";

/// The window staged in slot `n` (1 is the first), or `None` when the slot is empty or there is no
/// such slot.
pub fn staged(s: &Settings, n: usize) -> Option<&str> {
    let key = SLOT_KEYS.get(n.checked_sub(1)?)?;
    s.str(key).filter(|w| !w.is_empty())
}

/// The presented slot, 1 to [`SLOTS`]. A missing or out-of-range value reads as 1.
pub fn presented(s: &Settings) -> usize {
    s.i64(PRESENTED).and_then(|n| usize::try_from(n).ok()).filter(|n| (1..=SLOTS).contains(n)).unwrap_or(1)
}

/// The next slot that holds a window after the presented one (`forward`), or before it,
/// wrapping around. It is the presented slot itself when that is the only slot that holds a window,
/// and `None` when every slot is empty.
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
