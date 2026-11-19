use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use craft_app_audio::fake::{Behavior, FakeBackend, app};
use craft_app_audio::{AppAudioError, AppKey, CaptureState, Discovery, Target};
use studiocraft_media::clock::MasterClock;
use studiocraft_model::{FieldKind, Settings};
use studiocraft_plugin::{Plugin, Source, SourceIo};

use super::{AppAudioSource, Env, Handle, Machine, State};

struct Stub {
    target: Target,
    state: Cell<CaptureState>,
    stalled: Cell<bool>,
    error: Cell<Option<AppAudioError>>,
}

impl Handle for Rc<Stub> {
    fn state(&self) -> CaptureState {
        self.state.get()
    }
    fn target(&self) -> Target {
        self.target
    }
    fn stalled(&self, _now_ms: u64) -> bool {
        self.stalled.get()
    }
    fn take_error(&self) -> Option<AppAudioError> {
        self.error.take()
    }
}

#[derive(Default)]
struct World {
    unsupported: Option<String>,
    list_error: Option<String>,
    pid: Option<u32>,
    refuse: Option<AppAudioError>,
    opened: Vec<Rc<Stub>>,
}

impl Env for World {
    type H = Rc<Stub>;
    fn unsupported(&self) -> Option<String> {
        self.unsupported.clone()
    }
    fn list_error(&self) -> Option<String> {
        self.list_error.clone()
    }
    fn find(&self, _key: &AppKey) -> Option<u32> {
        self.pid
    }
    fn open(&mut self, _key: &AppKey, target: Target) -> Result<Option<Rc<Stub>>, AppAudioError> {
        if let Some(e) = self.refuse.take() {
            return Err(e);
        }
        let s = Rc::new(Stub { target, state: Cell::new(CaptureState::Starting), stalled: Cell::new(false), error: Cell::new(None) });
        self.opened.push(Rc::clone(&s));
        Ok(Some(s))
    }
}

fn key() -> AppKey {
    AppKey::from_exe("player.exe").unwrap()
}

fn failed(text: &str) -> Result<(), AppAudioError> {
    Err(AppAudioError::Failed(text.into()))
}

#[test]
fn without_an_application_the_source_stays_unbound() {
    let mut m: Machine<Rc<Stub>> = Machine::new(None);
    let mut w = World { pid: Some(5), ..World::default() };
    assert_eq!(m.step(&mut w, Instant::now(), 0), Ok(()));
    assert_eq!(m.state, State::Unbound);
    assert!(w.opened.is_empty());
}

#[test]
fn a_chosen_application_on_a_system_without_support_says_why() {
    let mut m = Machine::new(Some(key()));
    let mut w = World { unsupported: Some("needs a newer system".into()), pid: Some(5), ..World::default() };
    assert_eq!(m.step(&mut w, Instant::now(), 0), Err(AppAudioError::Unsupported("needs a newer system".into())));
    assert_eq!(m.state, State::Waiting);
    assert!(w.opened.is_empty());
    // Without a chosen application there is nothing to report.
    let mut none: Machine<Rc<Stub>> = Machine::new(None);
    assert_eq!(none.step(&mut w, Instant::now(), 0), Ok(()));
}

#[test]
fn a_chosen_application_that_is_not_running_waits_silently() {
    let mut m = Machine::new(Some(key()));
    let mut w = World::default();
    assert_eq!(m.step(&mut w, Instant::now(), 0), Ok(()));
    assert_eq!(m.state, State::Waiting);
    w.pid = Some(5);
    m.step(&mut w, Instant::now(), 0).unwrap();
    assert_eq!(m.state, State::Binding);
    assert_eq!(w.opened[0].target, Target { pid: 5 });
    w.opened[0].state.set(CaptureState::Running);
    m.step(&mut w, Instant::now(), 0).unwrap();
    assert_eq!(m.state, State::Capturing);
}

#[test]
fn a_restarted_application_is_captured_again_under_its_new_process() {
    let mut m = Machine::new(Some(key()));
    let mut w = World { pid: Some(5), ..World::default() };
    m.step(&mut w, Instant::now(), 0).unwrap();
    w.opened[0].state.set(CaptureState::Running);
    m.step(&mut w, Instant::now(), 0).unwrap();
    // It quits: silence, without an error.
    w.pid = None;
    assert_eq!(m.step(&mut w, Instant::now(), 0), Ok(()));
    assert_eq!(m.state, State::Waiting);
    assert!(m.handle.is_none());
    // It starts again as process 9.
    w.pid = Some(9);
    m.step(&mut w, Instant::now(), 0).unwrap();
    assert_eq!(w.opened.len(), 2);
    assert_eq!(w.opened[1].target, Target { pid: 9 });
    // A new process while the old capture still runs also rebinds.
    w.opened[1].state.set(CaptureState::Running);
    w.pid = Some(11);
    m.step(&mut w, Instant::now(), 0).unwrap();
    assert_eq!(w.opened.len(), 3);
    assert_eq!(w.opened[2].target.pid, 11);
}

#[test]
fn a_failed_capture_backs_off_before_it_opens_again() {
    let mut m = Machine::new(Some(key()));
    let mut w = World { pid: Some(5), refuse: Some(AppAudioError::Failed("no audio".into())), ..World::default() };
    let t0 = Instant::now();
    assert_eq!(m.step(&mut w, t0, 0), failed("no audio"));
    assert_eq!(m.state, State::Backoff { until: t0 + Duration::from_millis(250) });
    assert_eq!(m.step(&mut w, t0 + Duration::from_millis(100), 0), failed("no audio"), "the error stays shown while the wait runs");
    assert!(w.opened.is_empty(), "nothing opens while the wait runs");
    assert_eq!(m.step(&mut w, t0 + Duration::from_millis(250), 0), failed("no audio"), "and while the next capture starts");
    assert_eq!(w.opened.len(), 1);
    // Started with no audio yet: the error stays until audio arrives.
    w.opened[0].state.set(CaptureState::Started);
    assert_eq!(m.step(&mut w, t0 + Duration::from_millis(255), 0), failed("no audio"));
    w.opened[0].state.set(CaptureState::Running);
    assert_eq!(m.step(&mut w, t0 + Duration::from_millis(260), 0), Ok(()), "a running capture clears it");
    // The capture fails later: the error is reported once, and the next wait is longer.
    w.opened[0].state.set(CaptureState::Failed);
    w.opened[0].error.set(Some(AppAudioError::Failed("device lost".into())));
    let t1 = t0 + Duration::from_millis(300);
    assert_eq!(m.step(&mut w, t1, 0), failed("device lost"));
    assert_eq!(m.state, State::Backoff { until: t1 + Duration::from_millis(250) }, "a capture that ran starts the waits over");
    // It quits while the source waits: silence, and the error is gone at once.
    w.pid = None;
    assert_eq!(m.step(&mut w, t1 + Duration::from_millis(100), 0), Ok(()));
    assert_eq!(m.state, State::Waiting);
}

#[test]
fn repeated_failures_wait_longer_each_time() {
    let mut m = Machine::new(Some(key()));
    let mut w = World { pid: Some(5), ..World::default() };
    let mut t = Instant::now();
    let mut waits = Vec::new();
    for _ in 0..6 {
        w.refuse = Some(AppAudioError::Failed("busy".into()));
        assert!(m.step(&mut w, t, 0).is_err());
        let State::Backoff { until } = m.state else { panic!("{:?}", m.state) };
        waits.push((until - t).as_millis());
        t = until;
    }
    assert_eq!(waits, [250, 500, 1000, 2000, 5000, 5000]);
}

#[test]
fn an_application_that_quits_and_starts_again_opens_without_the_earlier_waits() {
    let mut m = Machine::new(Some(key()));
    let mut w = World { pid: Some(5), ..World::default() };
    let mut t = Instant::now();
    for _ in 0..4 {
        w.refuse = Some(AppAudioError::Failed("busy".into()));
        assert!(m.step(&mut w, t, 0).is_err());
        let State::Backoff { until } = m.state else { panic!("{:?}", m.state) };
        t = until;
    }
    // It quits, then starts again and fails once more: the wait starts over at 250 ms.
    w.pid = None;
    assert_eq!(m.step(&mut w, t, 0), Ok(()));
    w.pid = Some(6);
    w.refuse = Some(AppAudioError::Failed("busy".into()));
    assert!(m.step(&mut w, t, 0).is_err());
    assert_eq!(m.state, State::Backoff { until: t + Duration::from_millis(250) });
}

#[test]
fn a_refusal_waits_longest_at_once_and_says_so() {
    let mut m = Machine::new(Some(key()));
    let mut w = World { pid: Some(5), refuse: Some(AppAudioError::Denied("access denied".into())), ..World::default() };
    let t0 = Instant::now();
    assert_eq!(m.step(&mut w, t0, 0), Err(AppAudioError::Denied("access denied".into())));
    assert_eq!(m.state, State::Backoff { until: t0 + Duration::from_millis(5000) });
}

#[test]
fn a_target_that_exited_before_the_capture_started_is_not_an_error() {
    let mut m = Machine::new(Some(key()));
    let mut w = World { pid: Some(5), refuse: Some(AppAudioError::NotRunning("process 5".into())), ..World::default() };
    let t0 = Instant::now();
    assert_eq!(m.step(&mut w, t0, 0), Ok(()));
    assert!(matches!(m.state, State::Backoff { .. }), "it waits for the list to catch up");
    assert_eq!(m.step(&mut w, t0 + Duration::from_millis(100), 0), Ok(()));
}

#[test]
fn a_list_that_cannot_be_read_is_reported() {
    let mut m = Machine::new(Some(key()));
    let mut w = World { list_error: Some("the list failed".into()), ..World::default() };
    assert_eq!(m.step(&mut w, Instant::now(), 0), Err(AppAudioError::List("the list failed".into())));
    assert_eq!(m.state, State::Waiting);
    w.list_error = None;
    assert_eq!(m.step(&mut w, Instant::now(), 0), Ok(()));
}

#[test]
fn a_failed_scan_is_reported_while_the_last_list_still_holds_the_application() {
    let mut m = Machine::new(Some(key()));
    let mut w = World { pid: Some(5), ..World::default() };
    m.step(&mut w, Instant::now(), 0).unwrap();
    w.opened[0].state.set(CaptureState::Running);
    assert_eq!(m.step(&mut w, Instant::now(), 0), Ok(()));
    // A scan fails and the list keeps the application: the capture goes on, and the failure shows.
    w.list_error = Some("the list failed".into());
    assert_eq!(m.step(&mut w, Instant::now(), 0), Err(AppAudioError::List("the list failed".into())));
    assert_eq!(m.state, State::Capturing);
    assert_eq!(w.opened.len(), 1, "the running capture is kept");
    w.list_error = None;
    assert_eq!(m.step(&mut w, Instant::now(), 0), Ok(()));
}

#[test]
fn a_stalled_capture_is_dropped_and_opened_again() {
    let mut m = Machine::new(Some(key()));
    let mut w = World { pid: Some(5), ..World::default() };
    let t0 = Instant::now();
    m.step(&mut w, t0, 0).unwrap();
    w.opened[0].state.set(CaptureState::Running);
    w.opened[0].stalled.set(true);
    let r = m.step(&mut w, t0, 0);
    assert!(r.is_err_and(|e| e.to_string().contains("stopped responding")));
    assert!(m.handle.is_none(), "the stalled capture was handed off");
    assert_eq!(Rc::strong_count(&w.opened[0]), 1);
    let _ = m.step(&mut w, t0 + Duration::from_millis(250), 0);
    assert_eq!(w.opened.len(), 2);
}

#[test]
fn a_new_choice_starts_over() {
    let mut m = Machine::new(Some(key()));
    let mut w = World { pid: Some(5), refuse: Some(AppAudioError::Failed("x".into())), ..World::default() };
    let t0 = Instant::now();
    let _ = m.step(&mut w, t0, 0);
    assert!(m.rebind(Some(AppKey::from_exe("other.exe").unwrap())));
    assert_eq!(m.state, State::Unbound);
    m.step(&mut w, t0, 0).unwrap();
    assert_eq!(w.opened.len(), 1, "the new choice opens at once, without the old wait");
    assert!(!m.rebind(Some(AppKey::from_exe("other.exe").unwrap())));
    assert!(m.handle.is_some(), "the same choice keeps its capture");
    assert!(m.rebind(None));
    assert!(m.handle.is_none());
}

#[test]
fn closing_the_gate_waits_a_bounded_time_for_a_callback_inside_it() {
    use std::sync::mpsc::sync_channel;
    let gate = Arc::new(super::Gate::new());
    assert!(Arc::new(super::Gate::new()).close(), "an idle gate closes at once");
    let (entered_tx, entered_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel::<()>(1);
    let g = Arc::clone(&gate);
    let inside = std::thread::spawn(move || {
        g.pass(|| {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
        })
    });
    entered_rx.recv().unwrap();
    // The callback stays inside: closing gives up after its bound instead of blocking the caller.
    let started = Instant::now();
    assert!(!gate.close(), "the callback is still inside");
    assert!(started.elapsed() >= super::CLOSE_WAIT);
    release_tx.send(()).unwrap();
    inside.join().unwrap();
    assert_eq!(gate.0.load(std::sync::atomic::Ordering::Acquire), super::LEFT, "the callback has left");
    let ran = Cell::new(false);
    gate.pass(|| ran.set(true));
    assert!(!ran.get(), "a closed gate lets nothing through");
}

#[test]
fn the_schema_lists_running_applications_and_keeps_a_saved_one() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true), app("browser.exe", 41, false)]);
    let discovery = Discovery::new(Arc::new(fake.clone()));
    let source = AppAudioSource::new(discovery.clone());
    let usage = discovery.acquire();
    let list = discovery.wait_newer(0, Duration::from_secs(10));
    assert_eq!(source.lists_stamp(), Some(list.generation));
    let values = |s: &studiocraft_model::SettingsSchema| -> Vec<String> {
        match &s.fields[0].kind {
            FieldKind::Choice { options } => options.iter().map(|o| o.value.as_str().unwrap_or_default().to_string()).collect(),
            _ => Vec::new(),
        }
    };
    assert_eq!(values(&source.schema()), ["", r"browser.exe|c:\apps\browser.exe", r"player.exe|c:\apps\player.exe"]);
    let labels = |s: &studiocraft_model::SettingsSchema| -> Vec<String> {
        match &s.fields[0].kind {
            FieldKind::Choice { options } => options.iter().map(|o| o.label.clone()).collect(),
            _ => Vec::new(),
        }
    };
    let mut saved = Settings::new();
    saved.set("app", "game.exe");
    let kept = source.schema_for(&saved);
    assert_eq!(values(&kept), ["", r"browser.exe|c:\apps\browser.exe", r"player.exe|c:\apps\player.exe", "game.exe"]);
    assert_eq!(labels(&kept).last().map(String::as_str), Some("game"), "a kept application is shown by its program name");
    // A saved value that finds a listed application in another letter case, or by its file name
    // after the program moved, takes that entry's place: one entry, selected, with the value kept.
    for raw in [r"Player.EXE|C:\Apps\Player.exe", r"player.exe|d:\moved\player.exe", "PLAYER.EXE"] {
        saved.set("app", raw);
        let s = source.schema_for(&saved);
        assert_eq!(values(&s), ["", r"browser.exe|c:\apps\browser.exe", raw], "{raw}");
        assert_eq!(labels(&s)[2], "player", "{raw}");
    }
    drop(usage);
}

#[test]
fn a_late_callback_of_the_earlier_choice_does_not_reach_the_new_one() {
    use std::sync::mpsc::sync_channel;
    let wait = Duration::from_secs(10);
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true), app("other.exe", 41, true)]);
    fake.set_behavior(41, Behavior::Quiet);
    let discovery = Discovery::new(Arc::new(fake.clone()));
    let usage = discovery.acquire();
    discovery.wait_newer(0, wait);
    let io = SourceIo::new(MasterClock::new());
    let mut instance = super::Instance::new(discovery.clone(), None);
    // A callback of the earlier choice enters the gate and is held there past the close wait.
    let gate = Arc::new(super::Gate::new());
    instance.gate = Some(Arc::clone(&gate));
    let (entered_tx, entered_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel::<()>(1);
    let mut feed = crate::AudioFeed::new(48_000, 2, Arc::clone(&io.audio), io.clock);
    let g = Arc::clone(&gate);
    let late = std::thread::spawn(move || {
        g.pass(|| {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
            feed.push(&[0.5; 2048 * 2]);
        })
    });
    entered_rx.recv().unwrap();
    let other = Settings::new().with("app", "other.exe");
    studiocraft_plugin::SourceInstance::update(&mut instance, &other, &io).unwrap();
    assert!(instance.machine.handle.is_none(), "no new capture opens while the old callback is inside");
    release_tx.send(()).unwrap();
    late.join().unwrap();
    // The late callback queued two blocks after the choice changed; the mixer reads none of them.
    assert!(io.audio.pop().is_none(), "the mixer reads nothing while the old callback is closing");
    assert_eq!(io.audio.queued(), 0);
    instance.check(&io).unwrap();
    assert!(instance.machine.handle.is_some(), "the new choice opens once the old callback has left");
    // The blocks were thrown away, and the mixer reads the queue again.
    assert!(io.audio.pop().is_none(), "they are thrown away before the new capture opens");
    io.audio.push(io.audio.block());
    assert!(io.audio.pop().is_some(), "the queue is read again");
    drop(instance);
    drop(usage);
}

#[test]
fn a_new_process_is_not_captured_while_the_old_callback_is_inside() {
    use std::sync::mpsc::sync_channel;
    let wait = Duration::from_secs(10);
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    fake.set_behavior(40, Behavior::Quiet);
    fake.set_behavior(42, Behavior::Quiet);
    let discovery = Discovery::new(Arc::new(fake.clone()));
    let usage = discovery.acquire();
    let first = discovery.wait_newer(0, wait);
    let io = SourceIo::new(MasterClock::new());
    let mut instance = super::Instance::new(discovery.clone(), AppKey::from_exe("player.exe"));
    instance.check(&io).unwrap();
    let gate = instance.gate.clone().expect("the capture has a gate");
    let (entered_tx, entered_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel::<()>(1);
    let g = Arc::clone(&gate);
    let late = std::thread::spawn(move || {
        g.pass(|| {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
        })
    });
    entered_rx.recv().unwrap();
    // The application restarts as process 42 while the old callback is inside its gate.
    fake.set_apps(vec![app("player.exe", 42, true)]);
    discovery.refresh();
    discovery.wait_newer(first.generation, wait);
    instance.check(&io).unwrap();
    assert!(instance.machine.handle.is_none(), "process 42 is not captured yet");
    release_tx.send(()).unwrap();
    late.join().unwrap();
    instance.check(&io).unwrap();
    let deadline = Instant::now() + wait;
    while fake.opened().len() < 2 {
        assert!(Instant::now() < deadline, "process 42 is captured once the old callback has left");
        std::thread::yield_now();
    }
    assert_eq!(fake.opened(), [Target { pid: 40 }, Target { pid: 42 }]);
    drop(instance);
    drop(usage);
}

#[test]
fn an_open_form_replaces_a_stuck_discovery_thread() {
    let wait = Duration::from_secs(10);
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    let discovery = Discovery::new(Arc::new(fake.clone()));
    let source = AppAudioSource::new(discovery.clone());
    let _ = source.schema();
    let first = discovery.fresh_list(wait).unwrap();
    discovery.set_stall_limits(craft_app_audio::ACTIVATE_MS, craft_app_audio::WEDGE_MS, 0);
    fake.hold_next_scan();
    discovery.refresh();
    assert!(fake.wait_scan_stalled(&discovery, 1, wait), "the stuck scan shows as a stall");
    fake.set_apps(vec![app("browser.exe", 41, true)]);
    // The form's next stamp check starts a new thread, whose list the form then reads.
    let _ = source.lists_stamp();
    assert_eq!(discovery.wait_newer(first.generation, wait).apps[0].name, "browser");
    fake.release_scan();
}

#[test]
fn a_saved_value_that_is_not_an_application_value_still_loads() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    let source = AppAudioSource::new(Discovery::new(Arc::new(fake.clone())));
    let io = SourceIo::new(MasterClock::new());
    let mut instance = source.create(&Settings::new().with("app", serde_json::json!(5)), &io).expect("the collection still loads");
    assert_eq!(instance.produce(&io).err().map(|e| e.to_string()).as_deref(), Some(super::BAD_APP));
    assert!(instance.update(&Settings::new().with("app", "player.exe"), &io).is_ok(), "a valid choice ends the error");
    assert!(instance.produce(&io).is_ok());
}

#[test]
fn a_saved_application_round_trips_through_a_collection_file() {
    // Version 1 of the `app` setting: `name.exe` for a file name listed once, or
    // `name.exe|path` with the lower-case path for a file name listed more than once.
    use studiocraft_model::{SceneCollection, SourceConfig, SourceId, format};
    let source = AppAudioSource::new(Discovery::new(Arc::new(FakeBackend::new())));
    for value in ["player.exe", r"game.exe|d:\b\game.exe"] {
        let mut c = SceneCollection::new("Test", "Scene");
        let id = SourceId(c.alloc_id());
        c.sources.push(SourceConfig {
            id,
            kind: "app_audio".into(),
            kind_version: source.version(),
            name: "Player".into(),
            settings: Settings::new().with("app", value),
            filters: Vec::new(),
            audio: Default::default(),
        });
        let text = format::write_collection(&c).unwrap();
        let back = format::read_collection(&text, "Scene").unwrap();
        let s = &back.source(id).unwrap().settings;
        assert_eq!(s.str("app"), Some(value));
        assert_eq!(super::checked_key(s).unwrap(), AppKey::decode(value));
        assert_eq!(back.source(id).unwrap().kind_version, 1);
    }
}

#[test]
fn programs_with_the_same_file_name_show_their_paths() {
    let fake = FakeBackend::new();
    let mut a = app("game.exe", 40, true);
    a.path = r"D:\A\game.exe".into();
    let mut b = app("game.exe", 41, false);
    b.path = r"D:\B\game.exe".into();
    fake.set_apps(vec![a, b, app("player.exe", 42, false)]);
    let discovery = Discovery::new(Arc::new(fake.clone()));
    let source = AppAudioSource::new(discovery.clone());
    let _ = source.schema();
    assert!(discovery.fresh_list(Duration::from_secs(10)).is_some());
    let FieldKind::Choice { options } = &source.schema().fields[0].kind else { panic!("app is a choice") };
    let labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, ["Choose an application", r"game (D:\A\game.exe)", r"game (D:\B\game.exe)", "player"]);
}

#[test]
fn the_schema_lists_applications_before_any_source_exists_without_waiting() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    let discovery = Discovery::new(Arc::new(fake.clone()));
    let source = AppAudioSource::new(discovery.clone());
    // The first scan sticks in the system: the request answers at once with no applications.
    fake.hold_next_scan();
    let FieldKind::Choice { options } = &source.schema().fields[0].kind else { panic!("app is a choice") };
    assert_eq!(options.len(), 1, "only the empty choice before the first scan");
    assert!(discovery.running(), "the request keeps the list current for a while");
    let stamp = source.lists_stamp();
    assert!(fake.wait_scan_held(Duration::from_secs(10)));
    fake.release_scan();
    assert!(discovery.fresh_list(Duration::from_secs(10)).is_some());
    assert_ne!(source.lists_stamp(), stamp, "the form sees a new stamp and reads the schema again");
    let FieldKind::Choice { options } = &source.schema().fields[0].kind else { panic!("app is a choice") };
    let values: Vec<&str> = options.iter().map(|o| o.value.as_str().unwrap_or_default()).collect();
    assert_eq!(values, ["", r"player.exe|c:\apps\player.exe"]);
}

#[test]
fn the_window_is_given_a_fixed_message_for_a_failed_capture() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    fake.set_behavior(40, Behavior::Fail("the device answered 0x88890004".into()));
    let discovery = Discovery::new(Arc::new(fake.clone()));
    let usage = discovery.acquire();
    discovery.wait_newer(0, Duration::from_secs(10));
    let io = SourceIo::new(MasterClock::new());
    let mut instance = super::Instance::new(discovery.clone(), AppKey::from_exe("player.exe"));
    instance.check(&io).unwrap();
    let handle = instance.machine.handle.as_ref().expect("the capture opened");
    assert_eq!(handle.wait_ended(Duration::from_secs(10)), CaptureState::Failed);
    let err = instance.check(&io).unwrap_err().to_string();
    assert_eq!(err, super::CAPTURE_FAILED, "the system's text stays out of the window");
    assert_eq!(instance.logged.as_deref(), Some("the device answered 0x88890004"), "and goes to the log");
    // A check without an error (here: the new choice is not running) ends the failure, so the
    // same text from a later capture is logged again.
    let mut other = Settings::new();
    other.set("app", "other.exe");
    let _ = studiocraft_plugin::SourceInstance::update(&mut instance, &other, &io);
    assert_eq!(instance.logged, None);
    // The capture limit and a system without support keep their own texts; a refusal and a list
    // that cannot be read have fixed messages of their own.
    assert_eq!(instance.shown_error(AppAudioError::Limit), AppAudioError::Limit.to_string());
    assert_eq!(instance.shown_error(AppAudioError::Unsupported("needs a newer system".into())), "needs a newer system");
    assert_eq!(instance.shown_error(AppAudioError::Denied("0x80070005".into())), super::CAPTURE_DENIED);
    assert_eq!(instance.logged.as_deref(), Some("0x80070005"));
    assert_eq!(instance.shown_error(AppAudioError::List("no endpoints".into())), super::LIST_FAILED);
    assert_eq!(instance.shown_error(AppAudioError::Busy("player.exe".into())), super::CAPTURE_FAILED);
    drop(instance);
    drop(usage);
}

#[test]
fn an_application_value_that_is_not_a_key_is_refused() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    let source = AppAudioSource::new(Discovery::new(Arc::new(fake.clone())));
    let io = SourceIo::new(MasterClock::new());
    let with = |v: serde_json::Value| Settings::new().with("app", v);
    // Missing, null and empty mean no application.
    assert!(source.create(&Settings::new(), &io).is_ok());
    assert!(source.create(&with(serde_json::Value::Null), &io).is_ok());
    assert!(source.create(&with(" ".into()), &io).is_ok());
    // Anything else that is not a key is reported, not taken as no application.
    for bad in [
        serde_json::json!(5),
        serde_json::json!("|c:\\x.exe"),
        serde_json::json!("player.exe|"),
        serde_json::json!("c:\\other\\player.exe"),
        serde_json::json!("x".repeat(5000)),
    ] {
        let mut i = source.create(&with(bad.clone()), &io).unwrap();
        let err = i.produce(&io).err().map(|e| e.to_string()).unwrap_or_default();
        assert_eq!(err, super::BAD_APP, "{bad}");
    }
    // An update with a bad value is refused and keeps the current choice.
    let mut instance = source.create(&with("player.exe".into()), &io).unwrap();
    assert!(instance.update(&with(serde_json::json!(5)), &io).is_err());
    let r = instance.update(&with("player.exe".into()), &io);
    assert!(r.is_ok(), "{r:?}");
}

#[test]
fn the_only_setting_is_the_application() {
    // Process loopback records a process tree or everything except it, so the source offers no
    // switch that would turn the capture into the second.
    let schema = super::schema(&craft_app_audio::AppList::default(), None);
    let keys: Vec<&str> = schema.fields.iter().map(|f| f.key.as_str()).collect();
    assert_eq!(keys, ["app"]);
}

#[test]
fn every_error_the_window_shows_has_a_message_row_in_every_catalog() {
    let shown = [
        super::CAPTURE_FAILED.to_string(),
        super::BAD_APP.to_string(),
        super::CAPTURE_DENIED.to_string(),
        super::LIST_FAILED.to_string(),
        craft_app_audio::NOT_AVAILABLE.to_string(),
        craft_app_audio::NEEDS_NEWER_WINDOWS.to_string(),
        AppAudioError::Limit.to_string(),
    ];
    let locales = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ui-egui/i18n/locales");
    let mut catalogs = 0;
    for entry in std::fs::read_dir(&locales).unwrap() {
        let path = entry.unwrap().path().join("studiocraft.tsv");
        let text = std::fs::read_to_string(&path).unwrap();
        for msg in &shown {
            let row =
                text.lines().find_map(|l| l.strip_prefix("@msg\t").and_then(|r| r.strip_prefix(msg.as_str())).and_then(|r| r.strip_prefix('\t')));
            assert!(row.is_some_and(|t| !t.trim().is_empty()), "{} has no row for {msg:?}", path.display());
        }
        catalogs += 1;
    }
    assert_eq!(catalogs, 19);
}

#[test]
fn a_capture_dropped_after_the_application_quits_closes_its_gate() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    fake.set_behavior(40, Behavior::Deliver { frames: 480, value: 0.25 });
    let discovery = Discovery::new(Arc::new(fake.clone()));
    let usage = discovery.acquire();
    let first = discovery.wait_newer(0, Duration::from_secs(10));
    let io = SourceIo::new(MasterClock::new());
    let mut instance = super::Instance::new(discovery.clone(), AppKey::from_exe("player.exe"));
    instance.check(&io).unwrap();
    let gate = instance.gate.clone().expect("the capture has a gate");
    fake.set_apps(Vec::new());
    discovery.refresh();
    discovery.wait_newer(first.generation, Duration::from_secs(10));
    instance.check(&io).unwrap();
    assert!(instance.machine.handle.is_none());
    assert!(instance.gate.is_none());
    let ran = Cell::new(false);
    gate.pass(|| ran.set(true));
    assert!(!ran.get(), "the dropped capture's callback no longer reaches the mixer");
    drop(instance);
    drop(usage);
}

#[test]
fn a_source_captures_its_application_into_the_mixer() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    fake.set_behavior(40, Behavior::Deliver { frames: 480, value: 0.25 });
    let discovery = Discovery::new(Arc::new(fake.clone()));
    let usage = discovery.acquire();
    discovery.wait_newer(0, Duration::from_secs(10));
    let source = AppAudioSource::new(discovery.clone());
    let io = SourceIo::new(MasterClock::new());
    let audio = Arc::clone(&io.audio);
    let mut settings = Settings::new();
    settings.set("app", "player.exe");
    let mut instance = source.create(&settings, &io).unwrap();
    // The first packet arrives before the capture reports running.
    let deadline = Instant::now() + Duration::from_secs(10);
    while discovery.captures().first().is_none_or(|c| c.state != CaptureState::Running) {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(fake.opened(), [Target { pid: 40 }]);
    // 480 frames do not fill a 1024-frame block; wait for later packets.
    let block = loop {
        if let Some(b) = audio.pop() {
            break b;
        }
        assert!(Instant::now() < deadline, "a block reached the mixer");
        std::thread::yield_now();
    };
    assert_eq!(block.samples[0], 0.25);
    instance.produce(&io).unwrap();
    assert_eq!(discovery.capture_count(), 1);
    // Another choice while blocks of the old one wait in the queue: 5 more 480-frame packets fill
    // at least two more 1024-frame blocks. Once the old capture's thread has ended, none of its
    // audio is queued.
    let packets = |d: &Discovery| d.captures().first().map_or(0, |c| c.packets);
    let wanted = packets(&discovery) + 5;
    while packets(&discovery) < wanted {
        assert!(Instant::now() < deadline, "more packets arrived");
        std::thread::yield_now();
    }
    let mut other = Settings::new();
    other.set("app", "other.exe");
    instance.update(&other, &io).unwrap();
    while discovery.capture_count() > 0 {
        assert!(Instant::now() < deadline, "the old capture ended");
        std::thread::yield_now();
    }
    assert!(audio.pop().is_none(), "the earlier application's audio was thrown away");
    drop(instance);
    drop(usage);
}
