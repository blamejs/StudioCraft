//! The canvas follows the display its size came from: the displays are looked at on another
//! thread, and the window takes the answer when it arrives, without waiting for it.

use std::sync::Arc;
use std::time::{Duration, Instant};

use studiocraft_platform::display::DisplayInfo;

use crate::forms::{Reply, try_read_off};
use crate::store::CanvasDisplay;

/// How often the displays are looked at while the canvas follows one.
pub const LOOK: Duration = Duration::from_secs(2);

/// Lists the displays; see `studiocraft_platform::display::displays`.
pub type Lister = Arc<dyn Fn() -> Vec<DisplayInfo> + Send + Sync>;

/// Looks at the displays every [`LOOK`], and at once when the display the window is on reports
/// another size.
pub struct Watch {
    lister: Lister,
    /// A look running on its own thread.
    look: Option<Arc<Reply<Vec<DisplayInfo>>>>,
    /// When to look next.
    next: Instant,
    /// The size of the display the window is on at the last call.
    hint: Option<(u32, u32)>,
    /// That size changed while a look ran, so the next look starts as soon as it ends.
    again: bool,
}

impl Watch {
    pub fn new(lister: Lister) -> Watch {
        Watch { lister, look: None, next: Instant::now(), hint: None, again: false }
    }

    /// The displays, listed here and now.
    pub fn list(&self) -> Vec<DisplayInfo> {
        (self.lister)()
    }

    /// Start a look when one is due, and return the displays a look found once it has ended.
    /// `hint` is the size of the display the window is on. This never waits for a look.
    pub fn poll(&mut self, now: Instant, hint: Option<(u32, u32)>) -> Option<Vec<DisplayInfo>> {
        if hint != self.hint {
            self.hint = hint;
            self.next = now;
            self.again = self.look.is_some();
        }
        let Some(look) = &self.look else {
            if now >= self.next {
                // When no thread can be started, the window does not list the displays itself;
                // it tries again later.
                self.look = try_read_off(self.lister.clone());
                if self.look.is_none() {
                    self.next = now + LOOK;
                }
            }
            return None;
        };
        let found = look.ended()?;
        self.look = None;
        self.next = if std::mem::take(&mut self.again) { now } else { now + LOOK };
        // A look that stopped without a list finds nothing; the next one is tried as usual.
        found
    }

    /// Whether a look is running.
    #[cfg(test)]
    pub fn looking(&self) -> bool {
        self.look.is_some()
    }
}

/// The display `link` names in `list`: the one with its id and name, or else the only one with
/// its name. An id that now belongs to a display with another name is not used, since ids can be
/// given out again when the app starts again or a display is connected again.
pub fn find<'a>(list: &'a [DisplayInfo], link: &CanvasDisplay) -> Option<&'a DisplayInfo> {
    list.iter()
        .find(|d| !link.id.is_empty() && d.id == link.id && d.name == link.name)
        .or_else(|| only(list.iter().filter(|d| !link.name.is_empty() && d.name == link.name)))
}

/// The listed display the window is on, `size` being that display's size in pixels: the only
/// listed display of that size. Two displays of one size can't be told apart.
pub fn of_size(list: &[DisplayInfo], size: (u32, u32)) -> Option<&DisplayInfo> {
    only(list.iter().filter(|d| (d.width, d.height) == size))
}

fn only<T>(mut it: impl Iterator<Item = T>) -> Option<T> {
    let first = it.next()?;
    it.next().is_none().then_some(first)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn d(id: &str, name: &str, width: u32, height: u32) -> DisplayInfo {
        DisplayInfo { id: id.into(), name: name.into(), width, height, primary: false }
    }

    fn link(id: &str, name: &str) -> CanvasDisplay {
        CanvasDisplay { profile: "Untitled".into(), id: id.into(), name: name.into(), width: 1920, height: 1080 }
    }

    #[test]
    fn a_display_is_found_by_its_id_or_its_only_name() {
        let list = [d("1", "Desk", 1920, 1080), d("2", "Desk", 1920, 1080), d("3", "Wall", 3840, 2160)];
        let id = |l: &CanvasDisplay| find(&list, l).map(|d| d.id.clone());
        assert_eq!(id(&link("2", "Desk")), Some("2".into()), "the id comes first");
        assert_eq!(id(&link("2", "Wall")), Some("3".into()), "an id given to another display is not used");
        assert_eq!(id(&link("7", "Wall")), Some("3".into()));
        assert_eq!(id(&link("7", "Desk")), None, "two displays have that name");
        assert_eq!(id(&link("", "")), None);
        assert_eq!(find(&list[..0], &link("1", "Desk")), None);
        assert!(find(&[d("", "", 1, 1)], &link("", "")).is_none(), "an empty id or name matches nothing");
        assert_eq!(of_size(&list, (3840, 2160)).map(|d| d.id.as_str()), Some("3"));
        assert_eq!(of_size(&list, (1920, 1080)), None);
        assert_eq!(of_size(&list, (1280, 720)), None);
    }

    /// Poll until the running look ends.
    fn wait(w: &mut Watch, now: Instant, hint: Option<(u32, u32)>) -> Option<Vec<DisplayInfo>> {
        loop {
            let found = w.poll(now, hint);
            if !w.looking() {
                return found;
            }
            std::thread::yield_now();
        }
    }

    #[test]
    fn the_displays_are_looked_at_on_time_and_when_the_window_display_changes() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        // Each look waits until the test lets it end.
        let (go, gate) = std::sync::mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        let mut w = Watch::new(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = gate.lock().unwrap().recv();
            vec![d("1", "Desk", 1920, 1080)]
        }));
        let t = Instant::now();
        assert!(w.poll(t, None).is_none(), "a look starts and its answer comes later");
        for _ in 0..100 {
            assert!(w.poll(t, None).is_none(), "nothing waits for the look");
        }
        go.send(()).unwrap();
        assert_eq!(wait(&mut w, t, None).map(|l| l.len()), Some(1));
        assert!(w.poll(t + LOOK - Duration::from_millis(1), None).is_none());
        assert!(!w.looking(), "not before it is time");
        // The display the window is on reports another size: look at once.
        let t1 = t + Duration::from_millis(1);
        w.poll(t1, Some((2560, 1440)));
        assert!(w.looking());
        go.send(()).unwrap();
        assert!(wait(&mut w, t1, Some((2560, 1440))).is_some());
        assert!(w.poll(t1, Some((2560, 1440))).is_none());
        assert!(!w.looking());
        // A change while a look runs starts the next look as soon as it ends.
        let t2 = t + LOOK * 3;
        w.poll(t2, Some((2560, 1440)));
        assert!(w.looking());
        w.poll(t2, Some((3840, 2160)));
        go.send(()).unwrap();
        wait(&mut w, t2, Some((3840, 2160)));
        w.poll(t2, Some((3840, 2160)));
        assert!(w.looking(), "the change during the look is looked at");
        go.send(()).unwrap();
        wait(&mut w, t2, Some((3840, 2160)));
        assert_eq!(calls.load(Ordering::SeqCst), 4);
    }
}
