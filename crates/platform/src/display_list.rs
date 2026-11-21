//! The display list on Linux: Wayland's outputs, or RandR's monitors under X11.
//!
//! Listing needs no permission and no picker. Capture still goes through the screen-cast portal.
//! The system is asked on a thread of its own, and a caller does not wait for it: each call
//! returns the last answer and starts a new query, so the answer reaches the next caller. One
//! query runs at a time.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, LazyLock, Mutex, PoisonError};

use crate::display::DisplayInfo;

/// The most displays read from one answer.
const MAX_DISPLAYS: usize = 64;
/// The most characters kept of a display's id or name.
const MAX_NAME: usize = 256;

/// The displays Wayland or X11 reported to the last query, in the order the system reported them,
/// or none before the first answer. Starts a new query unless one is running.
pub(crate) fn displays() -> Vec<DisplayInfo> {
    static LISTER: LazyLock<Lister> = LazyLock::new(|| Lister::new(Arc::new(query)));
    LISTER.list()
}

/// Wayland's outputs when a Wayland compositor answers, otherwise RandR's monitors, otherwise none.
fn query() -> Vec<DisplayInfo> {
    match wayland::outputs() {
        Some(list) => list,
        None => x11::monitors().unwrap_or_default(),
    }
}

type Query = Arc<dyn Fn() -> Vec<DisplayInfo> + Send + Sync>;

#[derive(Default)]
struct State {
    running: bool,
    /// Counts the answers, so a caller that waits can tell a new one arrived.
    answers: u64,
    last: Vec<DisplayInfo>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    answered: Condvar,
}

/// Runs a query on its own thread and hands out its latest answer.
struct Lister {
    query: Query,
    shared: Arc<Shared>,
}

impl Lister {
    fn new(query: Query) -> Lister {
        Lister { query, shared: Arc::default() }
    }

    /// The last answer. Starts a query unless one is running.
    fn list(&self) -> Vec<DisplayInfo> {
        let mut state = self.shared.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.running {
            state.running = true;
            let query = Arc::clone(&self.query);
            let shared = Arc::clone(&self.shared);
            let spawned = std::thread::Builder::new().name("display list".into()).spawn(move || {
                // A query that panics answers with no displays, so the next caller starts another.
                let list = catch_unwind(AssertUnwindSafe(&*query)).unwrap_or_default();
                let mut state = shared.state.lock().unwrap_or_else(PoisonError::into_inner);
                state.last = list;
                state.running = false;
                state.answers = state.answers.wrapping_add(1);
                shared.answered.notify_all();
            });
            if spawned.is_err() {
                state.running = false;
            }
        }
        state.last.clone()
    }
}

/// `text` cut to [`MAX_NAME`] characters.
fn clip(text: String) -> String {
    if text.chars().count() <= MAX_NAME { text } else { text.chars().take(MAX_NAME).collect() }
}

/// A side reported by the system as pixels, or `None` when it is not a positive number.
fn side(v: i64) -> Option<u32> {
    u32::try_from(v).ok().filter(|v| *v > 0)
}

mod wayland {
    use wayland_client::protocol::wl_output::{self, Transform, WlOutput};
    use wayland_client::protocol::wl_registry::{self, WlRegistry};
    use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

    use super::{MAX_DISPLAYS, clip, side};
    use crate::display::DisplayInfo;

    /// What an output reported about itself.
    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    pub(super) struct Output {
        /// The registry's name for the output, used as the id when the output has no name.
        pub global: u32,
        /// The connector, for example `DP-1` (version 4 of the output interface).
        pub connector: Option<String>,
        pub description: Option<String>,
        pub make: String,
        pub model: String,
        /// The current mode's size in the device's pixels, before the output's rotation.
        pub mode: Option<(i32, i32)>,
        /// The output is turned a quarter or three quarters, so its picture is the mode's height wide.
        pub turned: bool,
    }

    #[derive(Default)]
    struct State {
        outputs: Vec<Output>,
    }

    impl Dispatch<WlRegistry, ()> for State {
        fn event(state: &mut Self, registry: &WlRegistry, event: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
            if let wl_registry::Event::Global { name, interface, version } = event
                && interface == "wl_output"
                && state.outputs.len() < MAX_DISPLAYS
            {
                let index = state.outputs.len();
                state.outputs.push(Output { global: name, ..Output::default() });
                registry.bind::<WlOutput, _, _>(name, version.min(4), qh, index);
            }
        }
    }

    impl Dispatch<WlOutput, usize> for State {
        fn event(state: &mut Self, _: &WlOutput, event: wl_output::Event, index: &usize, _: &Connection, _: &QueueHandle<Self>) {
            let Some(o) = state.outputs.get_mut(*index) else { return };
            match event {
                wl_output::Event::Geometry { make, model, transform, .. } => {
                    o.make = make;
                    o.model = model;
                    o.turned = matches!(transform, WEnum::Value(Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270));
                }
                wl_output::Event::Mode { flags, width, height, .. } => {
                    let current = match flags {
                        WEnum::Value(f) => f.contains(wl_output::Mode::Current),
                        WEnum::Unknown(bits) => bits & 1 != 0,
                    };
                    if current {
                        o.mode = Some((width, height));
                    }
                }
                wl_output::Event::Name { name } => o.connector = Some(name),
                wl_output::Event::Description { description } => o.description = Some(description),
                _ => {}
            }
        }
    }

    /// The outputs, or `None` when no Wayland compositor answers.
    pub(super) fn outputs() -> Option<Vec<DisplayInfo>> {
        let conn = Connection::connect_to_env().ok()?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = State::default();
        // The first round trip announces the outputs, the second their modes and names.
        queue.roundtrip(&mut state).ok()?;
        queue.roundtrip(&mut state).ok()?;
        Some(state.outputs.iter().filter_map(info).collect())
    }

    /// An output as a display, or `None` when it reported no usable current mode.
    pub(super) fn info(o: &Output) -> Option<DisplayInfo> {
        let (w, h) = o.mode?;
        let (w, h) = (side(i64::from(w))?, side(i64::from(h))?);
        let (width, height) = if o.turned { (h, w) } else { (w, h) };
        let connector = o.connector.clone().filter(|c| !c.trim().is_empty());
        let id = connector.clone().unwrap_or_else(|| format!("output-{}", o.global));
        let made = format!("{} {}", o.make.trim(), o.model.trim()).trim().to_string();
        let name = o
            .description
            .clone()
            .filter(|d| !d.trim().is_empty())
            .or((!made.is_empty()).then_some(made))
            .or(connector)
            .unwrap_or_else(|| "Display".into());
        Some(DisplayInfo { id: clip(id), name: clip(name), width, height, primary: false })
    }
}

mod x11 {
    use x11rb::connection::Connection as _;
    use x11rb::protocol::randr::ConnectionExt as _;
    use x11rb::protocol::xproto::ConnectionExt as _;

    use super::{MAX_DISPLAYS, clip, side};
    use crate::display::DisplayInfo;

    /// The active monitors, or `None` when no X server answers or it lacks RandR 1.5.
    pub(super) fn monitors() -> Option<Vec<DisplayInfo>> {
        let (conn, screen) = x11rb::connect(None).ok()?;
        let root = conn.setup().roots.get(screen)?.root;
        conn.randr_query_version(1, 5).ok()?.reply().ok()?;
        let reply = conn.randr_get_monitors(root, true).ok()?.reply().ok()?;
        let list = reply
            .monitors
            .iter()
            .take(MAX_DISPLAYS)
            .enumerate()
            .filter_map(|(index, m)| {
                let name = conn.get_atom_name(m.name).ok().and_then(|c| c.reply().ok());
                let name = name.map(|r| String::from_utf8_lossy(&r.name).into_owned()).unwrap_or_default();
                monitor(index, name, m.primary, m.width, m.height)
            })
            .collect();
        Some(list)
    }

    /// A monitor as a display, or `None` when a side is zero.
    pub(super) fn monitor(index: usize, name: String, primary: bool, width: u16, height: u16) -> Option<DisplayInfo> {
        let (width, height) = (side(i64::from(width))?, side(i64::from(height))?);
        let name = clip(name.trim().to_string());
        let id = if name.is_empty() { format!("monitor-{index}") } else { name.clone() };
        let name = if name.is_empty() { "Display".to_string() } else { name };
        Some(DisplayInfo { id, name, width, height, primary })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    use super::*;

    fn screen(id: &str) -> DisplayInfo {
        DisplayInfo { id: id.into(), name: "Screen".into(), width: 1920, height: 1080, primary: false }
    }

    #[test]
    fn a_late_answer_does_not_hold_up_the_caller_and_is_kept_for_the_next() {
        let (release, released) = mpsc::channel::<Vec<DisplayInfo>>();
        let released = Mutex::new(released);
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let (start, started) = mpsc::channel::<()>();
        let start = Mutex::new(start);
        let lister = Lister::new(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            let _ = start.lock().unwrap().send(());
            released.lock().unwrap().recv().unwrap_or_default()
        }));

        // The query has not answered: the caller gets the last list (none yet) at once.
        assert!(lister.list().is_empty());
        started.recv().unwrap();
        // A second caller does not start a second query while the first runs.
        assert!(lister.list().is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // The late answer is what the next caller gets at once, and that caller asks again.
        release.send(vec![screen("DP-1")]).unwrap();
        wait_for_answers(&lister, 1);
        assert_eq!(lister.list(), vec![screen("DP-1")]);
        started.recv().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        release.send(vec![screen("DP-1"), screen("HDMI-A-1")]).unwrap();
        wait_for_answers(&lister, 2);
        assert_eq!(lister.list(), vec![screen("DP-1"), screen("HDMI-A-1")]);
        started.recv().unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    /// Wait until the lister has stored `n` answers.
    fn wait_for_answers(lister: &Lister, n: u64) {
        let state = lister.shared.state.lock().unwrap();
        drop(lister.shared.answered.wait_while(state, |s| s.answers < n).unwrap());
    }

    #[test]
    fn a_query_that_panics_answers_with_no_displays_and_the_next_call_asks_again() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        let lister = Lister::new(Arc::new(move || {
            if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("the first query fails");
            }
            vec![screen("eDP-1")]
        }));
        assert!(lister.list().is_empty());
        wait_for_answers(&lister, 1);
        assert!(lister.list().is_empty(), "the failed query answered with no displays");
        wait_for_answers(&lister, 2);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "the call after the failure asked again");
        assert_eq!(lister.list(), vec![screen("eDP-1")]);
    }

    #[test]
    fn a_wayland_output_is_listed_in_pixels_turned_as_the_output_is() {
        let o = wayland::Output {
            global: 41,
            connector: Some("DP-1".into()),
            description: Some("Studio monitor (DP-1)".into()),
            make: "Maker".into(),
            model: "M27".into(),
            mode: Some((3840, 2160)),
            turned: false,
        };
        let d = wayland::info(&o).unwrap();
        assert_eq!(d, DisplayInfo { id: "DP-1".into(), name: "Studio monitor (DP-1)".into(), width: 3840, height: 2160, primary: false });

        let turned = wayland::info(&wayland::Output { turned: true, ..o.clone() }).unwrap();
        assert_eq!((turned.width, turned.height), (2160, 3840));

        // Without a name the id comes from the registry and the name from the maker and model.
        let plain = wayland::info(&wayland::Output { connector: None, description: None, ..o.clone() }).unwrap();
        assert_eq!((plain.id.as_str(), plain.name.as_str()), ("output-41", "Maker M27"));
        let bare = wayland::Output { global: 7, mode: Some((1280, 720)), ..wayland::Output::default() };
        let bare = wayland::info(&bare).unwrap();
        assert_eq!((bare.id.as_str(), bare.name.as_str()), ("output-7", "Display"));

        // No current mode, or a side that is not positive, is not listed.
        assert_eq!(wayland::info(&wayland::Output { mode: None, ..o.clone() }), None);
        for bad in [(0, 1080), (1920, -1), (i32::MIN, i32::MIN)] {
            assert_eq!(wayland::info(&wayland::Output { mode: Some(bad), ..o.clone() }), None, "{bad:?}");
        }

        // A long name is cut.
        let long = wayland::info(&wayland::Output { connector: Some("x".repeat(10_000)), description: Some("y".repeat(10_000)), ..o }).unwrap();
        assert_eq!((long.id.chars().count(), long.name.chars().count()), (MAX_NAME, MAX_NAME));
    }

    #[test]
    fn an_x11_monitor_is_listed_with_its_primary_flag() {
        assert_eq!(
            x11::monitor(0, "HDMI-1".into(), true, 2560, 1440),
            Some(DisplayInfo { id: "HDMI-1".into(), name: "HDMI-1".into(), width: 2560, height: 1440, primary: true })
        );
        let unnamed = x11::monitor(3, " ".into(), false, 1024, 768).unwrap();
        assert_eq!((unnamed.id.as_str(), unnamed.name.as_str()), ("monitor-3", "Display"));
        assert_eq!(x11::monitor(0, "VGA-1".into(), false, 0, 768), None);
        assert_eq!(x11::monitor(0, "x".repeat(1000), false, 640, 480).map(|d| d.id.chars().count()), Some(MAX_NAME));
    }

    #[test]
    fn every_listed_display_has_a_size() {
        // Build servers have neither a Wayland compositor nor an X server; with one, each listed
        // display has a size.
        for d in displays() {
            assert!(d.width > 0 && d.height > 0, "{d:?}");
        }
    }
}
