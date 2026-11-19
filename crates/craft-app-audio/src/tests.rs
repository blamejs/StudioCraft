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
fn arranging_sorts_by_name_and_tells_same_named_files_apart() {
    let mut a = app("game.exe", 10, false);
    a.path = r"D:\A\game.exe".into();
    let mut b = app("game.exe", 11, true);
    b.path = r"D:\B\game.exe".into();
    let mut apps = vec![app("viewer.exe", 3, true), b, app("beta.exe", 4, false), a];
    AppList::arrange(&mut apps);
    let names: Vec<&str> = apps.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["beta", "game", "game", "viewer"], "the order does not depend on which applications play");
    assert_eq!(apps[1].key.path.as_deref(), Some(r"d:\a\game.exe"));
    assert_eq!(apps[2].key.path.as_deref(), Some(r"d:\b\game.exe"));
    assert_eq!(apps[0].key.path, None, "a unique name keeps a key without a path");
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
    assert_eq!(apps[5].key_id, apps[5].key.id(), "the key's id is worked out with the row");
    let now = now_ms();
    assert_eq!(peaks.get(row, apps[5].key_id, now), None, "a row with no reading yet reads nothing");
    peaks.set(usize::from(row), 0.5, now);
    assert_eq!(peaks.get(row, apps[5].key_id, now), Some(0.5));
    assert_eq!(peaks.get(row, apps[6].key.id(), now), None, "another app's row reads nothing");
    // App 0 quits: its row is freed and given to the first app without one.
    let gone = apps.remove(0);
    discovery::assign_slots(&peaks, &mut slots, &mut apps);
    assert_eq!(apps[4].slot, Some(row), "an app keeps its row across scans");
    assert_eq!(peaks.get(gone.slot.unwrap(), gone.key.id(), now), None);
    assert!(apps[63].slot.is_some(), "the freed row went to the next app");
    peaks.set(usize::from(row), f32::NAN, now);
    assert_eq!(peaks.get(row, apps[4].key_id, now), Some(0.0), "a reading that is not a number is stored as 0");
}

#[test]
fn a_level_left_from_a_reading_that_stopped_reads_nothing() {
    let peaks = PeakTable::default();
    let key = AppKey::from_exe("player.exe").unwrap();
    peaks.assign(3, key.id());
    peaks.set(3, 0.75, 1_000);
    assert_eq!(peaks.get(3, key.id(), 1_000 + PEAK_FRESH_MS), Some(0.75), "current within the fresh time");
    assert_eq!(peaks.get(3, key.id(), 1_001 + PEAK_FRESH_MS), None, "an old level is not reported as the current one");
    assert_eq!(peaks.get(3, 0, 1_000), None, "id 0 names no application");
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
    fake.set_apps(vec![app("player.exe", 40, true)]);
    d.refresh();
    let deadline = Instant::now() + WAIT;
    while fake.scans() <= scans {
        assert!(Instant::now() < deadline, "the refresh scanned again");
        std::thread::yield_now();
    }
    assert_eq!(d.generation(), first.generation);
}

#[test]
fn a_burst_of_wakes_scans_at_most_once_per_quarter_second() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    let d = discovery(&fake);
    let _usage = d.acquire();
    d.wait_newer(0, WAIT);
    let scans = fake.scans();
    let started = Instant::now();
    // A device or session event storm.
    for _ in 0..200 {
        d.refresh();
        std::thread::yield_now();
    }
    let elapsed = started.elapsed().as_millis();
    let allowed = 1 + usize::try_from(elapsed / 250).unwrap_or(usize::MAX) + 1;
    assert!(fake.scans() - scans <= allowed, "{} scans in {elapsed} ms", fake.scans() - scans);
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
    assert_eq!(d.peak(&list.apps[0]), None, "no reading before the list is shown, which is not the same as silence");
    d.set_visible(true);
    assert!(d.wait_peak_passes(1, WAIT));
    assert_eq!(d.peak(&list.apps[0]), Some(0.25));
}

#[test]
fn a_meter_with_no_reading_shows_no_level_rather_than_its_last_one() {
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
    assert_eq!(d.peak(&list.apps[0]), None, "a meter that gives no reading is not the same as silence");
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
    let names: Vec<String> = d.fresh_list(WAIT).unwrap().apps.iter().map(|a| a.name.clone()).collect();
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
    let current = d.fresh_list(WAIT).unwrap();
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
    assert_eq!(d.support().reason.as_deref(), Some("needs a newer system"));
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
fn a_fresh_list_is_not_waited_for_with_a_zero_timeout() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    let d = discovery(&fake);
    fake.hold_next_scan();
    let _usage = d.acquire();
    assert!(fake.wait_scan_held(WAIT));
    // The first scan is stuck: the caller gets no list instead of waiting.
    assert!(d.fresh_list(Duration::ZERO).is_none());
    fake.release_scan();
    assert_eq!(d.fresh_list(WAIT).map(|l| l.apps.len()), Some(1));
}

#[test]
fn a_stuck_discovery_thread_is_replaced_once() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    let d = discovery(&fake);
    let _usage = d.acquire();
    let first = d.fresh_list(WAIT).unwrap();
    // Any call longer than 0 ms counts as stuck from here on.
    d.set_stall_limits(ACTIVATE_MS, WEDGE_MS, 0);
    fake.hold_next_scan();
    d.refresh();
    assert!(fake.wait_scan_stalled(&d, 1, WAIT), "the stuck scan shows as a stall");
    // The browser starts while the first thread is stuck; the thread that replaces it lists it.
    fake.set_apps(vec![app("browser.exe", 41, true)]);
    assert!(d.recover(), "a new thread starts beside the stuck one");
    let second = d.wait_newer(first.generation, WAIT);
    assert_eq!(second.apps[0].name, "browser");
    // The new thread sticks as well: no third thread starts while the first is still stuck.
    fake.hold_next_scan();
    d.refresh();
    assert!(fake.wait_scan_stalled(&d, 2, WAIT), "the second stuck scan shows as a stall");
    assert!(!d.recover(), "at most one stuck thread is left beside the running one");
    fake.release_scan();
    assert!(fake.wait_watchers_dropped(1, WAIT), "the first stuck thread ends once its call returns");
}

#[test]
fn a_start_put_off_behind_stuck_threads_has_no_fresh_list() {
    let fake = FakeBackend::new();
    fake.set_apps(vec![app("player.exe", 40, true)]);
    let d = discovery(&fake);
    let usage = d.acquire();
    assert!(d.fresh_list(WAIT).is_some());
    d.set_stall_limits(ACTIVATE_MS, WEDGE_MS, 0);
    fake.hold_next_scan();
    d.refresh();
    assert!(fake.wait_scan_stalled(&d, 1, WAIT), "the stuck scan shows as a stall");
    assert!(d.recover());
    assert!(d.fresh_list(WAIT).is_some(), "the replacement scans once");
    fake.hold_next_scan();
    d.refresh();
    assert!(fake.wait_scan_stalled(&d, 2, WAIT), "the second stuck scan shows as a stall");
    // Both threads are alive, so the next start waits for one of them to end.
    drop(usage);
    let _usage = d.acquire();
    assert!(!d.running(), "the start is put off");
    assert!(d.fresh_list(Duration::ZERO).is_none(), "the stopped run's list is not current");
    fake.release_scan();
    assert!(d.fresh_list(WAIT).is_some(), "the start put off runs once a thread ends");
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
    assert_eq!(cap.take_error(), Some(AppAudioError::Failed("the process has no audio".into())));
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
fn a_refusal_and_an_exited_target_are_told_apart_from_other_failures() {
    let fake = FakeBackend::new();
    fake.set_behavior(9, Behavior::Deny("access denied".into()));
    fake.set_behavior(10, Behavior::Exit);
    let d = discovery(&fake);
    let key = AppKey::from_exe("a.exe").unwrap();
    let denied = d.open_capture(key.clone(), Target { pid: 9 }, Box::new(|_| {})).unwrap();
    assert_eq!(denied.wait(WAIT), CaptureState::Failed);
    assert_eq!(denied.take_error(), Some(AppAudioError::Denied("access denied".into())));
    let exited = d.open_capture(key, Target { pid: 10 }, Box::new(|_| {})).unwrap();
    assert_eq!(exited.wait(WAIT), CaptureState::Failed);
    assert!(matches!(exited.take_error(), Some(AppAudioError::NotRunning(_))));
}

#[test]
fn a_started_capture_with_no_audio_yet_is_not_running() {
    let fake = FakeBackend::new();
    fake.set_behavior(9, Behavior::Quiet);
    let d = discovery(&fake);
    let cap = d.open_capture(AppKey::from_exe("a.exe").unwrap(), Target { pid: 9 }, Box::new(|_| {})).unwrap();
    let deadline = Instant::now() + WAIT;
    while cap.state() == CaptureState::Starting {
        assert!(Instant::now() < deadline, "the stream started");
        std::thread::yield_now();
    }
    assert_eq!(cap.state(), CaptureState::Started, "no packet has arrived");
    assert_eq!(d.captures()[0].state, CaptureState::Started);
    assert_eq!(cap.packets(), 0);
}

#[test]
fn a_capture_whose_device_goes_away_fails_after_it_ran() {
    let fake = FakeBackend::new();
    fake.set_behavior(9, Behavior::FailAfter { packets: 3, error: "the device was removed".into() });
    let d = discovery(&fake);
    let cap = d.open_capture(AppKey::from_exe("a.exe").unwrap(), Target { pid: 9 }, Box::new(|_| {})).unwrap();
    assert_eq!(cap.wait_ended(WAIT), CaptureState::Failed);
    assert_eq!(cap.packets(), 3);
    assert_eq!(cap.take_error(), Some(AppAudioError::Failed("the device was removed".into())));
}

#[test]
fn an_activation_gets_longer_than_a_started_capture_before_it_counts_as_stalled() {
    let fake = FakeBackend::new();
    fake.set_behavior(9, Behavior::Hang);
    let d = discovery(&fake);
    let cap = d.open_capture(AppKey::from_exe("a.exe").unwrap(), Target { pid: 9 }, Box::new(|_| {})).unwrap();
    assert!(fake.wait_hanging(1, WAIT));
    let now = now_ms();
    assert_eq!(cap.state(), CaptureState::Starting);
    assert!(!cap.stalled(now + WEDGE_MS + 500), "a slow activation is not torn down after the started bound");
    assert!(cap.stalled(now + ACTIVATE_MS + 500));
    fake.release_hangs();
    assert_eq!(cap.wait_ended(WAIT), CaptureState::Ended);
}

#[test]
fn stuck_capture_threads_of_different_applications_are_bounded() {
    let fake = FakeBackend::new();
    let d = discovery(&fake);
    d.set_stall_limits(0, WEDGE_MS, DISCOVERY_WEDGE_MS);
    for i in 0..MAX_STUCK_CAPTURES {
        let pid = 100 + u32::try_from(i).unwrap();
        fake.set_behavior(pid, Behavior::Hang);
        let key = AppKey::from_exe(&format!("app{i}.exe")).unwrap();
        let cap = d.open_capture(key, Target { pid }, Box::new(|_| {})).unwrap();
        assert!(fake.wait_hanging(i + 1, WAIT));
        while !cap.stalled(now_ms()) {
            std::thread::yield_now();
        }
        drop(cap);
    }
    assert_eq!(d.capture_count(), 0, "stuck captures no longer count against the limit");
    // Another application would add one more stuck thread: no capture starts until one ends.
    let next = d.open_capture(AppKey::from_exe("other.exe").unwrap(), Target { pid: 20 }, Box::new(|_| {}));
    assert!(matches!(next, Err(AppAudioError::Limit)), "{:?}", next.err());
    assert_eq!(fake.hanging(), MAX_STUCK_CAPTURES, "no further thread was started");
    fake.release_hangs();
}

#[test]
fn a_stuck_capture_gives_back_its_place_and_is_not_started_again_beside_itself() {
    let fake = FakeBackend::new();
    fake.set_behavior(9, Behavior::Hang);
    let d = discovery(&fake);
    // Activations count as stuck after 0 ms here.
    d.set_stall_limits(0, WEDGE_MS, DISCOVERY_WEDGE_MS);
    let key = AppKey::from_exe("a.exe").unwrap();
    let cap = d.open_capture(key.clone(), Target { pid: 9 }, Box::new(|_| {})).unwrap();
    assert!(fake.wait_hanging(1, WAIT));
    let deadline = Instant::now() + WAIT;
    while !cap.stalled(now_ms()) {
        assert!(Instant::now() < deadline, "the hung activation counts as stalled");
        std::thread::yield_now();
    }
    assert_eq!(d.capture_count(), 1);
    drop(cap);
    assert_eq!(d.capture_count(), 0, "the owner let go of a stuck capture: it no longer counts against the limit");
    assert!(d.captures().is_empty(), "nor is it reported as a running capture");
    // The same application is not captured again while its earlier thread is stuck.
    let again = d.open_capture(key.clone(), Target { pid: 9 }, Box::new(|_| {}));
    assert!(matches!(again, Err(AppAudioError::Busy(_))), "{:?}", again.err());
    assert_eq!(fake.hanging(), 1, "no second thread was started");
    // Other applications still capture.
    let other = d.open_capture(AppKey::from_exe("b.exe").unwrap(), Target { pid: 20 }, Box::new(|_| {})).unwrap();
    assert_eq!(other.wait(WAIT), CaptureState::Running);
    assert_eq!(d.capture_count(), 1);
    // Once the stuck call returns, its thread ends without counting itself off a second time.
    fake.release_hangs();
    let deadline = Instant::now() + WAIT;
    let next = loop {
        match d.open_capture(key.clone(), Target { pid: 9 }, Box::new(|_| {})) {
            Err(AppAudioError::Busy(_)) => {
                assert!(Instant::now() < deadline, "the stuck thread ended");
                std::thread::yield_now();
            }
            r => break r,
        }
    };
    let next = next.unwrap();
    assert_eq!(next.wait_ended(WAIT), CaptureState::Ended, "a released hang returns at once");
    drop(next);
    drop(other);
    let deadline = Instant::now() + WAIT;
    while d.capture_count() != 0 {
        assert!(Instant::now() < deadline, "every place was given back once");
        std::thread::yield_now();
    }
    assert_eq!(d.capture_count(), 0);
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
    assert_eq!(Discovery::system().support(), s);
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
