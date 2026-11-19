use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::mpsc::sync_channel;
use std::time::{Duration, Instant};

use crate::fake::{Behavior, FakeBackend, app};
use crate::*;

/// The longest a test waits for a thread's handshake before it fails.
const WAIT: Duration = Duration::from_secs(10);

fn discovery(fake: &FakeBackend) -> Discovery {
    Discovery::new(Arc::new(fake.clone()))
}

#[test]
fn a_key_is_the_lower_case_file_name_and_round_trips() {
    let k = AppKey::from_exe(r"C:\Program Files\Music\Player.EXE").unwrap();
    assert_eq!(k.name, "player.exe");
    assert_eq!(AppKey::decode(&k.encode()), Some(k.clone()));
    let p = k.clone().with_path(r"C:\Program Files\Music\Player.exe");
    assert_eq!(p.encode(), r"player.exe|c:\program files\music\player.exe");
    assert_eq!(AppKey::decode(&p.encode()), Some(p.clone()));
    assert!(AppKey::from_exe("  ").is_none());
    assert!(AppKey::decode("").is_none());
    assert!(AppKey::decode(&"x".repeat(5000)).is_none(), "an oversized name is refused");
    // Only the forms `encode` writes are read.
    assert!(AppKey::decode("player.exe|").is_none(), "a `|` with no path");
    assert!(AppKey::decode(r"C:\other\player.exe").is_none(), "a full path in place of the name");
    assert!(AppKey::decode(r"c:\other\player.exe|c:\other\player.exe").is_none());
    // A program at a long path, up to Windows' longest, saves a choice that reads back.
    let mut deep = crate::fake::app("deep.exe", 1, false);
    deep.path = format!(r"C:\{}\deep.exe", "d".repeat(4000));
    assert_eq!(AppKey::decode(&deep.choice()), Some(deep.key.clone().with_path(&deep.path)));
    deep.path = format!(r"C:\{}\deep.exe", "d".repeat(100_000));
    assert_eq!(deep.choice(), "deep.exe", "a path longer than any Windows path is left out");
    // A request whose path grows past the limit when lower-cased is refused, not read as the name.
    assert_eq!(AppKey::decode(&format!("x.exe|{}", "\u{130}".repeat(32_768))), None);
    assert_ne!(k.id(), 0);
    assert_eq!(k.id(), AppKey::from_exe("player.exe").unwrap().id(), "the id is the same for the same key");
    assert_ne!(k.id(), p.id());
}

#[test]
fn a_key_with_a_path_matches_only_that_path() {
    let listed = AppKey::from_exe("game.exe").unwrap();
    let any = AppKey::from_exe("game.exe").unwrap();
    let one = any.clone().with_path(r"D:\Games\A\game.exe");
    assert!(any.matches(&listed, r"D:\Games\B\game.exe"));
    assert!(one.matches(&listed, r"d:\games\a\GAME.exe"));
    assert!(!one.matches(&listed, r"D:\Games\B\game.exe"));
}

#[test]
fn arranging_puts_playing_apps_first_and_tells_same_named_files_apart() {
    let mut a = app("game.exe", 10, false);
    a.path = r"D:\A\game.exe".into();
    let mut b = app("game.exe", 11, true);
    b.path = r"D:\B\game.exe".into();
    let mut apps = vec![app("zoom.exe", 3, false), a, app("beta.exe", 4, false), b];
    AppList::arrange(&mut apps);
    let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["game", "beta", "game", "zoom"]);
    assert_eq!(apps[0].key.path.as_deref(), Some(r"d:\b\game.exe"));
    assert_eq!(apps[2].key.path.as_deref(), Some(r"d:\a\game.exe"));
    assert_eq!(apps[1].key.path, None, "a unique name keeps a key without a path");
    let list = AppList { generation: 1, apps, error: None };
    assert_eq!(list.find(&AppKey::from_exe("game.exe").unwrap().with_path(r"D:\A\game.exe")).map(|a| a.pids[0]), Some(10));
}

#[test]
fn a_saved_choice_keeps_its_program_when_another_with_the_same_name_starts() {
    let mut a = app("game.exe", 10, false);
    a.path = r"D:\A\game.exe".into();
    let saved = AppKey::decode(&a.choice()).unwrap();
    assert_eq!(a.choice(), r"game.exe|d:\a\game.exe");
    let mut alone = vec![a.clone()];
    AppList::arrange(&mut alone);
    let list = AppList { generation: 1, apps: alone, error: None };
    assert_eq!(list.find(&saved).map(|x| x.pids[0]), Some(10));
    // Another game.exe starts, listed first because it plays.
    let mut b = app("game.exe", 11, true);
    b.path = r"D:\B\game.exe".into();
    let mut both = vec![a.clone(), b.clone()];
    AppList::arrange(&mut both);
    let list = AppList { generation: 2, apps: both, error: None };
    assert_eq!(list.find(&saved).map(|x| x.pids[0]), Some(10), "the saved path wins");
    assert_eq!(list.find(&AppKey::from_exe("game.exe").unwrap()), None, "a name alone does not guess between two");
    // The program moved after an update and runs alone: the name still finds it.
    let mut moved = a;
    moved.path = r"D:\A2\game.exe".into();
    let list = AppList { generation: 3, apps: vec![moved, app("other.exe", 3, false)], error: None };
    assert_eq!(list.find(&saved).map(|x| x.pids[0]), Some(10));
    // Two others with the name and neither at the saved path: none.
    let mut c = b.clone();
    c.path = r"D:\C\game.exe".into();
    c.pids = vec![12];
    let list = AppList { generation: 4, apps: vec![b.clone(), c], error: None };
    assert_eq!(list.find(&saved), None);
    // A folder name with letters outside ASCII matches after the same lower-casing.
    let mut e = app("game.exe", 13, false);
    e.path = r"D:\ÉQUIPE\game.exe".into();
    let saved_e = AppKey::decode(&e.choice()).unwrap();
    let list = AppList { generation: 5, apps: vec![b, e], error: None };
    assert_eq!(list.find(&saved_e).map(|x| x.pids[0]), Some(13));
}

#[test]
fn backoff_grows_to_five_seconds_and_resets() {
    let mut b = Backoff::default();
    let waits: Vec<u128> = (0..7).map(|_| b.fail().as_millis()).collect();
    assert_eq!(waits, [250, 500, 1000, 2000, 5000, 5000, 5000]);
    b.reset();
    assert_eq!(b.fail().as_millis(), 250);
}

#[test]
fn peak_rows_keep_their_apps_and_stop_at_64() {
    let peaks = PeakTable::default();
    let mut slots = BTreeMap::new();
    let mut apps: Vec<AppInfo> = (0..70).map(|i| app(&format!("app{i:02}.exe"), 100 + i, false)).collect();
    discovery::assign_slots(&peaks, &mut slots, &mut apps);
    assert_eq!(apps.iter().filter(|a| a.slot.is_some()).count(), PEAK_SLOTS);
    assert!(apps[64..].iter().all(|a| a.slot.is_none()), "the apps past the 64th have no meter");
    let row = apps[5].slot.unwrap();
    peaks.set(usize::from(row), 0.5);
    assert_eq!(peaks.get(row, &apps[5].key), Some(0.5));
    assert_eq!(peaks.get(row, &apps[6].key), None, "another app's row reads nothing");
    // App 0 quits: its row is freed and given to the first app without one.
    let gone = apps.remove(0);
    discovery::assign_slots(&peaks, &mut slots, &mut apps);
    assert_eq!(apps[4].slot, Some(row), "an app keeps its row across scans");
    assert_eq!(peaks.get(gone.slot.unwrap(), &gone.key), None);
    assert!(apps[63].slot.is_some(), "the freed row went to the next app");
    peaks.set(usize::from(row), f32::NAN);
    assert_eq!(peaks.get(row, &apps[4].key), Some(0.0), "a reading that is not a number is stored as 0");
}

#[test]
fn the_list_follows_apps_starting_quitting_and_restarting() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    let d = discovery(&fake);
    assert!(!d.running(), "nothing runs before a use");
    let usage = d.acquire();
    assert!(d.running());
    let first = d.wait_newer(0, WAIT);
    assert_eq!(first.apps.len(), 1);
    assert_eq!(first.apps[0].target_pid, 40);
    // The player quits and starts again with a new process id, and a browser starts.
    fake.set_apps(vec![app("player.exe", 77, false), app("browser.exe", 12, true)]);
    let second = d.wait_newer(first.generation, WAIT);
    assert_eq!(second.apps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["browser", "player"]);
    let player = second.find(&AppKey::from_exe("player.exe").unwrap()).unwrap();
    assert_eq!(player.target_pid, 77);
    assert_eq!(player.slot, first.apps[0].slot, "the restarted app keeps its meter row");
    fake.set_apps(Vec::new());
    assert!(d.wait_newer(second.generation, WAIT).apps.is_empty());
    drop(usage);
    assert!(!d.running(), "the thread stops with the last use");
}

#[test]
fn an_unchanged_list_is_not_published_again() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    let d = discovery(&fake);
    let _usage = d.acquire();
    let first = d.wait_newer(0, WAIT);
    let scans = fake.scans();
    d.refresh();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    // Wait for two more scans by asking for peaks, which the thread reads after each wake.
    d.set_visible(true);
    assert!(fake.wait_peak_reads(1, WAIT));
    assert!(fake.scans() > scans);
    assert_eq!(d.generation(), first.generation);
}

#[test]
fn peaks_are_read_only_while_visible() {
    let fake = FakeBackend::new();
    let player = app("player.exe", 40, true);
    fake.set_peak(&player.key, 0.25);
    fake.set_apps(vec![player]);
    let d = discovery(&fake);
    let _usage = d.acquire();
    let list = d.wait_newer(0, WAIT);
    assert_eq!(d.peak(&list.apps[0]), Some(0.0), "no reading before the list is shown");
    d.set_visible(true);
    assert!(d.wait_peak_passes(1, WAIT));
    assert_eq!(d.peak(&list.apps[0]), Some(0.25));
}

#[test]
fn a_meter_with_no_reading_shows_silence_rather_than_its_last_level() {
    let fake = FakeBackend::new();
    let player = app("player.exe", 40, true);
    fake.set_peak(&player.key, 0.25);
    fake.set_apps(vec![player.clone()]);
    let d = discovery(&fake);
    let _usage = d.acquire();
    let list = d.wait_newer(0, WAIT);
    d.set_visible(true);
    assert!(d.wait_peak_passes(1, WAIT));
    assert_eq!(d.peak(&list.apps[0]), Some(0.25));
    fake.clear_peak(&player.key);
    // A reading under way may have started before the clear; the one after it starts after.
    let passes = d.peak_passes();
    assert!(d.wait_peak_passes(passes + 2, WAIT));
    assert_eq!(d.peak(&list.apps[0]), Some(0.0));
}

#[test]
fn a_stuck_meter_does_not_hold_up_the_last_source_ending() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    let d = discovery(&fake);
    let usage = d.acquire();
    d.wait_newer(0, WAIT);
    fake.hold_next_peaks();
    d.set_visible(true);
    assert!(fake.wait_peaks_held(WAIT));
    // Dropping the last use takes the run lock; it returns while the meter is still stuck.
    let (done_tx, done_rx) = sync_channel(1);
    let dropper = std::thread::spawn(move || {
        drop(usage);
        let _ = done_tx.send(());
    });
    let returned = done_rx.recv_timeout(WAIT).is_ok();
    fake.release_peaks();
    let _ = dropper.join();
    assert!(returned, "dropping the last use waited for the meter");
    assert!(fake.wait_watchers_dropped(1, WAIT));
}
#[test]
fn a_lease_keeps_the_list_current_and_then_ends_by_itself() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    let d = discovery(&fake);
    d.lease(Duration::from_secs(600));
    assert_eq!(d.wait_newer(0, WAIT).apps.len(), 1);
    // A use that ends while the lease runs leaves the thread running.
    drop(d.acquire());
    assert!(d.running());
    // A lease that has ended stops the thread at its next check.
    let d2 = discovery(&fake);
    d2.lease(Duration::ZERO);
    d2.refresh();
    let deadline = Instant::now() + WAIT;
    while d2.running() {
        assert!(Instant::now() < deadline, "the thread stopped");
        std::thread::yield_now();
    }
}

#[test]
fn a_restarted_discovery_does_not_return_the_earlier_runs_list() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    let d = discovery(&fake);
    let usage = d.acquire();
    assert_eq!(d.wait_newer(0, WAIT).apps.len(), 1);
    drop(usage);
    assert!(!d.running());
    // The player quits while nothing runs; the next run lists what runs now.
    fake.set_apps(vec![app("browser.exe", 41, false)]);
    let _usage = d.acquire();
    let names: Vec<String> = d.list_now().apps.iter().map(|a| a.name.clone()).collect();
    assert_eq!(names, ["browser"]);
}

#[test]
fn a_run_stopped_during_its_scan_leaves_the_next_runs_list_and_peak_rows_alone() {
    let fake = FakeBackend::new();
    let player = app("player.exe", 40, true);
    fake.set_apps(vec![player.clone()]);
    let d = discovery(&fake);
    let first = d.acquire();
    let old = d.wait_newer(0, WAIT);
    // The first run's next scan reads the player and then sticks in the system.
    fake.hold_next_scan();
    d.refresh();
    assert!(fake.wait_scan_held(WAIT));
    // The player quits and the browser starts; the first run stops and a second one starts.
    let browser = app("browser.exe", 41, true);
    fake.set_peak(&browser.key, 0.5);
    fake.set_apps(vec![browser]);
    drop(first);
    let _second = d.acquire();
    let current = d.list_now();
    assert_eq!(current.apps[0].name, "browser");
    d.set_visible(true);
    assert!(d.wait_peak_passes(1, WAIT));
    // The stuck scan returns the player list into a run that has stopped.
    fake.release_scan();
    assert!(fake.wait_watchers_dropped(1, WAIT));
    let now = d.snapshot();
    assert_eq!(now.generation, current.generation, "the stopped run published nothing");
    assert_eq!(now.apps[0].name, "browser");
    assert_eq!(d.peak(&now.apps[0]), Some(0.5), "the second run's peak row is intact");
    assert_eq!(d.peak(&old.apps[0]), None, "the first run's row was freed");
}

#[test]
fn peaks_are_read_for_a_while_after_a_poll() {
    let fake = FakeBackend::new();
    let player = app("player.exe", 40, true);
    fake.set_peak(&player.key, 0.5);
    fake.set_apps(vec![player]);
    let d = discovery(&fake);
    let _usage = d.acquire();
    let list = d.wait_newer(0, WAIT);
    d.read_peaks_for(Duration::from_secs(60));
    assert!(d.wait_peak_passes(1, WAIT));
    assert_eq!(d.peak(&list.apps[0]), Some(0.5));
}

#[test]
fn a_system_without_support_starts_nothing_and_says_why() {
    let fake = FakeBackend::new();
    fake.set_support(Support::no("needs a newer system"));
    let d = discovery(&fake);
    let _usage = d.acquire();
    assert!(!d.running());
    let list = d.list_now();
    assert_eq!(list.error.as_deref(), Some("needs a newer system"));
    let key = AppKey::from_exe("player.exe").unwrap();
    let r = d.open_capture(key, Target { pid: 4 }, Box::new(|_| {}));
    assert!(matches!(r, Err(AppAudioError::Unsupported(_))));
    assert!(fake.opened().is_empty());
}

#[test]
fn a_watch_failure_is_published() {
    let fake = FakeBackend::new();
    fake.set_watch_error(Some("the audio service stopped"));
    let d = discovery(&fake);
    let _usage = d.acquire();
    let list = d.wait_newer(0, WAIT);
    assert_eq!(list.error.as_deref(), Some("the audio service stopped"));
}

#[test]
fn list_now_scans_once_when_nothing_runs() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    let d = discovery(&fake);
    let list = d.list_now();
    assert_eq!(list.apps.len(), 1);
    assert!(list.error.is_none());
    assert!(!d.running(), "listing once does not keep a thread");
}

#[test]
fn a_capture_delivers_and_stops_when_dropped() {
    let fake = FakeBackend::new();
    fake.set_behavior(9, Behavior::Deliver { frames: 480, value: 0.5 });
    let d = discovery(&fake);
    let (tx, rx) = sync_channel(1);
    let key = AppKey::from_exe("player.exe").unwrap();
    let cap = d
        .open_capture(
            key,
            Target { pid: 9 },
            Box::new(move |s: &[f32]| {
                let _ = tx.try_send((s.len(), s[0]));
            }),
        )
        .unwrap();
    assert_eq!(cap.wait(WAIT), CaptureState::Running);
    assert_eq!(rx.recv_timeout(WAIT).unwrap(), (960, 0.5));
    assert_eq!(d.capture_count(), 1);
    assert_eq!(d.captures()[0].pid, 9);
    assert_eq!(fake.opened(), [Target { pid: 9 }]);
    assert!(!cap.stalled(now_ms()));
    drop(cap);
    // The data callback is dropped with the thread, which closes the channel.
    while rx.recv_timeout(WAIT).is_ok() {}
}

#[test]
fn a_failed_capture_reports_its_error_once() {
    let fake = FakeBackend::new();
    fake.set_behavior(9, Behavior::Fail("the process has no audio".into()));
    let d = discovery(&fake);
    let cap = d.open_capture(AppKey::from_exe("a.exe").unwrap(), Target { pid: 9 }, Box::new(|_| {})).unwrap();
    assert_eq!(cap.wait(WAIT), CaptureState::Failed);
    assert_eq!(cap.take_error().as_deref(), Some("the process has no audio"));
    assert_eq!(cap.take_error(), None);
    assert!(!cap.stalled(now_ms() + 60_000), "a failed capture is not stalled");
    assert_eq!(d.capture_count(), 0, "the count goes down before the state changes");
}

#[test]
fn a_capture_that_stops_reporting_counts_as_stalled() {
    let fake = FakeBackend::new();
    fake.set_behavior(9, Behavior::Stall);
    let d = discovery(&fake);
    let cap = d.open_capture(AppKey::from_exe("a.exe").unwrap(), Target { pid: 9 }, Box::new(|_| {})).unwrap();
    assert_eq!(cap.wait(WAIT), CaptureState::Running);
    let now = now_ms();
    assert!(!cap.stalled(now));
    assert!(cap.stalled(now + WEDGE_MS + 500), "no report for longer than the limit");
    // Dropping it does not wait for the thread.
    drop(cap);
}

#[test]
fn at_most_32_captures_run_and_an_ended_one_frees_its_place() {
    let fake = FakeBackend::new();
    let d = discovery(&fake);
    let key = AppKey::from_exe("a.exe").unwrap();
    for pid in 0..MAX_CAPTURES as u32 {
        fake.set_behavior(pid, Behavior::Stall);
    }
    let caps: Vec<AppCapture> = (0..MAX_CAPTURES as u32).map(|pid| d.open_capture(key.clone(), Target { pid }, Box::new(|_| {})).unwrap()).collect();
    let r = d.open_capture(key.clone(), Target { pid: 99 }, Box::new(|_| {}));
    assert!(matches!(r, Err(AppAudioError::Limit)));
    // A stopped capture's thread ends, and its place is free once it has.
    caps[0].stop();
    assert_eq!(caps[0].wait_ended(WAIT), CaptureState::Ended);
    assert_eq!(d.capture_count(), MAX_CAPTURES - 1);
    assert_eq!(d.captures().len(), MAX_CAPTURES - 1, "an ended capture is not listed while its handle lives");
    fake.set_behavior(100, Behavior::End);
    let c = d.open_capture(key.clone(), Target { pid: 100 }, Box::new(|_| {})).unwrap();
    assert_eq!(c.wait_ended(WAIT), CaptureState::Ended);
    drop(caps);
}

#[test]
fn the_system_backend_answers() {
    let s = system_backend().support();
    if !s.supported {
        assert!(s.reason.as_deref().is_some_and(|r| !r.is_empty()));
    }
    let list = Discovery::system().list_now();
    if !s.supported {
        assert_eq!(list.error, s.reason);
        assert!(list.apps.is_empty());
    }
}

#[test]
fn support_and_lists_serialize_for_reports() {
    let v = serde_json::to_value(Support::no("x")).unwrap();
    assert_eq!(v["supported"], false);
    assert_eq!(v["permission"], "not_needed");
    let v = serde_json::to_value(app("player.exe", 1, true)).unwrap();
    assert_eq!(v["key"]["name"], "player.exe");
    assert!(v["key"].get("path").is_none());
}
