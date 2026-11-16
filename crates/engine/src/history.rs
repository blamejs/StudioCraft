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
    at: Instant,
}

/// The undo and redo stacks.
#[derive(Default)]
pub struct History {
    undo: Vec<Entry>,
    redo: Vec<(&'static str, SceneCollection)>,
}

impl History {
    /// Record a change from `before`. With a `key` equal to the last entry's within
    /// [`COALESCE`], the change joins that entry.
    pub fn record(&mut self, label: &'static str, before: SceneCollection, key: Option<String>) {
        self.redo.clear();
        let now = Instant::now();
        if let (Some(k), Some(last)) = (&key, self.undo.last_mut())
            && last.key.as_ref() == Some(k)
            && now.duration_since(last.at) < COALESCE
        {
            last.at = now;
            return;
        }
        self.undo.push(Entry { label, before, key, at: now });
        if self.undo.len() > LIMIT {
            self.undo.remove(0);
        }
    }

    /// Step back: returns the collection to restore, given the current one.
    pub fn undo(&mut self, current: &SceneCollection) -> Option<SceneCollection> {
        let e = self.undo.pop()?;
        self.redo.push((e.label, current.clone()));
        Some(e.before)
    }

    /// Step forward again.
    pub fn redo(&mut self, current: &SceneCollection) -> Option<SceneCollection> {
        let (label, after) = self.redo.pop()?;
        self.undo.push(Entry { label, before: current.clone(), key: None, at: Instant::now() });
        Some(after)
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
        self.redo.last().map(|(l, _)| *l)
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
