//! Undo and redo. Each entry keeps the scene collection as it was before a command; collections
//! are small (a few kilobytes), so a snapshot is cheaper and safer than per-command inverses.

use std::time::{Duration, Instant};

use studiocraft_model::SceneCollection;

/// Entries kept; the oldest go first.
pub const LIMIT: usize = 200;

/// Commands with the same coalescing key within this time make one undo step (a drag, a fader
/// move, typing in a properties field).
pub const COALESCE: Duration = Duration::from_millis(750);

struct Entry {
    label: &'static str,
    before: SceneCollection,
    key: Option<String>,
    /// When it was recorded; `None` without a clock ([`now`]).
    at: Option<Instant>,
    /// Kept through undo and redo ([`History::holds`]).
    step: u64,
    /// The redo steps that recording this entry cleared, for [`History::discard`]; `None` when
    /// the entry did not come from [`History::record`] or an undo or redo came after it.
    cleared: Option<Vec<Redo>>,
}

/// The time a step is recorded, for coalescing. A WebAssembly build for the browser has no clock
/// (`Instant::now` panics on `wasm32-unknown-unknown`), so there every change is its own step.
fn now() -> Option<Instant> {
    if cfg!(all(target_arch = "wasm32", target_os = "unknown")) { None } else { Some(Instant::now()) }
}

/// A step that can be redone: its label, the collection after it and its step id.
type Redo = (&'static str, SceneCollection, u64);

/// The undo and redo stacks.
#[derive(Default)]
pub struct History {
    undo: Vec<Entry>,
    redo: Vec<Redo>,
    /// The id the next new step gets.
    next_step: u64,
}

impl History {
    /// Record a change from `before`. With a `key` equal to the last entry's within
    /// [`COALESCE`], the change joins that entry; without a clock ([`now`]) it never does.
    pub fn record(&mut self, label: &'static str, before: SceneCollection, key: Option<String>) {
        self.record_at(label, before, key, now());
    }

    /// [`History::record`] at the time `now`. Without a time no change joins another.
    fn record_at(&mut self, label: &'static str, before: SceneCollection, key: Option<String>, now: Option<Instant>) {
        if let (Some(k), Some(last), Some(now)) = (&key, self.undo.last_mut(), now)
            && last.key.as_ref() == Some(k)
            && last.at.is_some_and(|at| now.duration_since(at) < COALESCE)
        {
            last.at = Some(now);
            self.redo.clear();
            return;
        }
        let cleared = Some(std::mem::take(&mut self.redo));
        self.next_step += 1;
        self.undo.push(Entry { label, before, key, at: now, step: self.next_step, cleared });
        if self.undo.len() > LIMIT {
            self.undo.remove(0);
        }
    }

    /// The id of the newest step that can be undone.
    pub fn last_step(&self) -> Option<u64> {
        self.undo.last().map(|e| e.step)
    }

    /// Whether the step `step` is in effect and can still be undone: it was recorded or redone
    /// and has not been undone since, nor dropped as one of the oldest.
    pub fn holds(&self, step: u64) -> bool {
        self.undo.iter().any(|e| e.step == step)
    }

    /// The ids of the undo steps newer than `step`, oldest first; `None` when `step` does not
    /// hold.
    pub fn steps_after(&self, step: u64) -> Option<Vec<u64>> {
        let at = self.undo.iter().position(|e| e.step == step)?;
        Some(self.undo[at + 1..].iter().map(|e| e.step).collect())
    }

    /// Step back: returns the collection to restore, given the current one.
    pub fn undo(&mut self, current: &SceneCollection) -> Option<SceneCollection> {
        let e = self.undo.pop()?;
        self.forget_cleared();
        self.redo.push((e.label, current.clone(), e.step));
        Some(e.before)
    }

    /// Step back and drop the step: returns the collection to restore. Redo does not offer the
    /// step. When no undo or redo came after the step was recorded, redo offers again the steps
    /// that recording it cleared; otherwise redo is empty.
    pub fn discard(&mut self) -> Option<SceneCollection> {
        let e = self.undo.pop()?;
        self.redo = e.cleared.unwrap_or_default();
        Some(e.before)
    }

    /// Step forward again.
    pub fn redo(&mut self, current: &SceneCollection) -> Option<SceneCollection> {
        let (label, after, step) = self.redo.pop()?;
        self.forget_cleared();
        self.undo.push(Entry { label, before: current.clone(), key: None, at: now(), step, cleared: None });
        Some(after)
    }

    /// After an undo or a redo, a discard gives back no redo steps.
    fn forget_cleared(&mut self) {
        for e in &mut self.undo {
            e.cleared = None;
        }
    }

    /// End coalescing: the next change starts a new entry even with the same key.
    pub fn seal(&mut self) {
        if let Some(last) = self.undo.last_mut() {
            last.key = None;
        }
    }

    pub fn undo_label(&self) -> Option<&'static str> {
        self.undo.last().map(|e| e.label)
    }

    pub fn redo_label(&self) -> Option<&'static str> {
        self.redo.last().map(|(l, _, _)| *l)
    }

    pub fn clear(&mut self) {
        self.undo.clear();
        self.redo.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(n: &str) -> SceneCollection {
        SceneCollection::new(n, "Scene")
    }

    #[test]
    fn undo_and_redo_walk_the_stack() {
        let mut h = History::default();
        h.record("a", named("0"), None);
        h.record("b", named("1"), None);
        let back = h.undo(&named("2")).unwrap();
        assert_eq!(back.name, "1");
        let back = h.undo(&back).unwrap();
        assert_eq!(back.name, "0");
        assert!(h.undo(&back).is_none());
        let fwd = h.redo(&back).unwrap();
        assert_eq!(fwd.name, "1");
        assert_eq!(h.redo_label(), Some("b"));
    }

    #[test]
    fn a_step_keeps_its_id_through_undo_and_redo() {
        let mut h = History::default();
        h.record("a", named("0"), None);
        let a = h.last_step().unwrap();
        h.record("b", named("1"), None);
        assert_ne!(h.last_step(), Some(a));
        assert!(h.holds(a));
        assert_eq!(h.steps_after(a), Some(vec![h.last_step().unwrap()]));
        assert_eq!(h.steps_after(999), None);
        h.undo(&named("2")).unwrap();
        h.undo(&named("1")).unwrap();
        assert!(!h.holds(a), "an undone step is not in effect");
        h.redo(&named("0")).unwrap();
        assert!(h.holds(a), "a redone step is in effect again");
        assert_eq!(h.last_step(), Some(a));
        h.undo(&named("1")).unwrap();
        h.record("c", named("0"), None);
        assert!(!h.holds(a), "a step the redo stack lost stays undone");
        h.clear();
        assert_eq!(h.last_step(), None);
    }

    #[test]
    fn a_discarded_step_is_not_offered_for_redo() {
        let mut h = History::default();
        h.record("a", named("0"), None);
        h.record("b", named("1"), None);
        assert_eq!(h.discard().map(|c| c.name), Some("1".into()));
        assert_eq!(h.redo_label(), None);
        assert_eq!(h.undo_label(), Some("a"));
        assert_eq!(h.discard().map(|c| c.name), Some("0".into()));
        assert!(h.discard().is_none(), "an empty history has nothing to drop");
    }

    #[test]
    fn discarding_the_newest_step_offers_the_redo_steps_it_cleared_again() {
        let mut h = History::default();
        h.record("a", named("0"), None);
        h.record("b", named("1"), None);
        let back = h.undo(&named("2")).unwrap();
        assert_eq!(h.redo_label(), Some("b"));
        h.record("layout", back, None);
        assert_eq!(h.redo_label(), None, "a new step clears redo");
        assert_eq!(h.discard().map(|c| c.name), Some("1".into()));
        assert_eq!(h.redo_label(), Some("b"), "redo is back");
        assert_eq!(h.redo(&named("1")).map(|c| c.name), Some("2".into()));
        // A step that did not come from a record has no redo steps to give back.
        let back = h.undo(&named("2")).unwrap();
        assert_eq!(back.name, "1");
        h.redo(&back).unwrap();
        h.discard().unwrap();
        assert_eq!(h.redo_label(), None);
    }

    #[test]
    fn discarding_steps_down_to_an_older_one_offers_the_redo_steps_it_cleared_again() {
        let mut h = History::default();
        h.record("a", named("0"), None);
        h.record("b", named("1"), None);
        let back = h.undo(&named("2")).unwrap();
        h.record("layout", back, None);
        h.record("placed", named("3"), None);
        h.discard().unwrap();
        assert_eq!(h.redo_label(), None, "the newest step cleared nothing");
        h.discard().unwrap();
        assert_eq!(h.redo_label(), Some("b"), "the older step gives back what it cleared");
        // An undo in between leaves nothing to give back.
        h.record("layout", named("1"), None);
        h.record("placed", named("3"), None);
        h.undo(&named("4")).unwrap();
        h.discard().unwrap();
        assert_eq!(h.redo_label(), None);
    }

    #[test]
    fn same_key_coalesces_and_seal_ends_it() {
        let mut h = History::default();
        h.record("move", named("0"), Some("item:1".into()));
        h.record("move", named("1"), Some("item:1".into()));
        assert_eq!(h.undo(&named("2")).unwrap().name, "0", "one step back to before the drag");
        let mut h = History::default();
        h.record("move", named("0"), Some("k".into()));
        h.seal();
        h.record("move", named("1"), Some("k".into()));
        assert_eq!(h.undo(&named("2")).unwrap().name, "1");
    }

    #[test]
    fn without_a_clock_each_change_is_its_own_step() {
        let mut h = History::default();
        h.record_at("move", named("0"), Some("k".into()), None);
        h.record_at("move", named("1"), Some("k".into()), None);
        assert_eq!(h.undo(&named("2")).unwrap().name, "1");
        // A step recorded without a time is not joined by a timed one either.
        let mut h = History::default();
        h.record_at("move", named("0"), Some("k".into()), None);
        h.record_at("move", named("1"), Some("k".into()), Some(Instant::now()));
        assert_eq!(h.undo(&named("2")).unwrap().name, "1");
    }

    #[test]
    fn a_new_change_clears_redo_and_the_limit_holds() {
        let mut h = History::default();
        for i in 0..(LIMIT + 10) {
            h.record("x", named(&i.to_string()), None);
        }
        assert_eq!(h.undo.len(), LIMIT);
        let _ = h.undo(&named("now"));
        h.record("y", named("z"), None);
        assert!(h.redo(&named("q")).is_none());
    }
}
