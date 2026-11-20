use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde_json::json;
use studiocraft_model::{Profile, SceneCollection};
use studiocraft_render::Gpu;

use super::*;

fn studio_with(gpu: Option<Gpu>, collection: SceneCollection, profile: Profile) -> Studio {
    let ffmpeg: FfmpegSetting = Arc::new(RwLock::new(String::new()));
    let registry = Arc::new(builtin_registry(&ffmpeg).unwrap());
    Studio::new(gpu, collection, profile, registry, ffmpeg)
}

/// One device for every test in this binary; opening many devices at once crashes some drivers.
fn shared_gpu() -> Option<Gpu> {
    static GPU: std::sync::OnceLock<Option<Gpu>> = std::sync::OnceLock::new();
    GPU.get_or_init(|| Gpu::headless().ok()).clone()
}

/// A studio without audio devices or a graphics device.
fn headless() -> Studio {
    studio_with(None, SceneCollection::new("Test", "Scene"), Profile::default())
}

#[test]
fn the_default_collection_hears_desktop_audio_and_the_microphone() {
    let c = default_collection("Mine");
    assert_eq!(c.scenes.len(), 1);
    assert_eq!(c.global_audio.len(), 2);
    let kinds: Vec<_> = c.sources.iter().map(|s| s.kind.as_str()).collect();
    assert_eq!(kinds, ["audio_output", "audio_input"]);
}

#[test]
fn every_builtin_plugin_registers() {
    let ffmpeg: FfmpegSetting = Arc::new(RwLock::new(String::new()));
    let reg = builtin_registry(&ffmpeg).unwrap();
    for t in ["fade", "cut", "fade_color", "swipe"] {
        assert!(reg.transition(t).is_ok(), "{t}");
    }
    for s in ["color", "image", "text", "media", "display_capture", "window_capture", "audio_input", "audio_output"] {
        assert!(reg.source(s).is_ok(), "{s}");
    }
    for o in ["file", "rtmp"] {
        assert!(reg.output(o).is_ok(), "{o}");
    }
}

#[test]
fn switching_scenes_runs_the_transition() {
    let mut s = headless();
    assert!(s.transition_progress().is_none());
    s.run("scene.add", &json!({"name": "Second"})).unwrap();
    assert!(s.transition_progress().is_some(), "a new scene goes on the program");
    std::thread::sleep(Duration::from_millis(350));
    s.tick();
    assert!(s.transition_progress().is_none());
    s.run("scene.select", &json!({"scene": "Scene"})).unwrap();
    let p = s.transition_progress().expect("a fade runs");
    assert!((0.0..=1.0).contains(&p));
    assert_eq!(s.state_json()["transitioning"], json!(true));
    std::thread::sleep(Duration::from_millis(350));
    s.tick();
    assert!(s.transition_progress().is_none(), "the 300 ms fade has ended");
}

#[test]
fn a_cut_switches_at_once() {
    let mut s = headless();
    s.run("transition.set", &json!({"kind": "cut"})).unwrap();
    s.run("scene.add", &json!({"name": "Second"})).unwrap();
    s.run("scene.select", &json!({"scene": "Second"})).unwrap();
    assert!(s.transition_progress().is_none());
}

#[test]
fn studio_mode_quick_transitions_apply_to_one_switch() {
    let mut s = headless();
    s.run("scene.add", &json!({"name": "Second"})).unwrap();
    std::thread::sleep(Duration::from_millis(350));
    s.tick();
    s.run("studio.toggle", &json!({})).unwrap();
    s.run("scene.select", &json!({"scene": "Scene"})).unwrap();
    assert!(s.transition_progress().is_none(), "studio mode: selecting changes the preview only");
    s.run("studio.transition", &json!({"kind": "cut"})).unwrap();
    assert!(s.transition_progress().is_none(), "a cut is instant");
    assert_eq!(s.collection().transition.kind, "fade", "the collection's transition stays");
    s.run("scene.select", &json!({"scene": "Second"})).unwrap();
    s.run("studio.transition", &json!({"ms": 1000})).unwrap();
    assert!(s.transition_progress().is_some(), "the collection's fade, for 1 s");
    std::thread::sleep(Duration::from_millis(400));
    s.tick();
    assert!(s.transition_progress().is_some(), "still running after the usual 300 ms");
}

#[test]
fn a_quick_transition_that_changes_nothing_is_dropped() {
    let mut s = headless();
    s.run("studio.toggle", &json!({})).unwrap();
    let revision = s.engine().revision();
    s.run("studio.transition", &json!({"kind": "cut", "ms": 50})).unwrap();
    assert_eq!(s.engine().revision(), revision, "the preview already shows the program");
    assert!(s.quick.is_none(), "the next switch uses the collection's transition");
}

#[test]
fn a_zero_duration_switches_at_once() {
    let mut s = headless();
    s.run("transition.duration", &json!({"ms": 0})).unwrap();
    s.run("scene.add", &json!({"name": "Second"})).unwrap();
    s.run("scene.select", &json!({"scene": "Second"})).unwrap();
    assert!(s.transition_progress().is_none());
}

#[test]
fn a_running_transition_unsettles_the_program_only() {
    let mut s = headless();
    assert!(s.view_settled(View::Program), "an empty scene with no transition");
    assert!(s.view_settled(View::Preview), "there is no preview outside studio mode");
    s.run("scene.add", &json!({"name": "Second"})).unwrap();
    assert!(!s.view_settled(View::Program), "the fade to the new scene runs");
    s.run("studio.toggle", &json!({})).unwrap();
    assert!(s.view_settled(View::Preview), "the preview scene has no items and no transition");
}

/// Ticks `s` until `view` is settled, for at most five seconds.
fn settle(s: &mut Studio, view: View) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        s.tick();
        if s.view_settled(view) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_source_with_nothing_to_show_still_settles() {
    let mut s = headless();
    s.run("transition.set", &json!({"kind": "cut"})).unwrap();
    s.run("source.add", &json!({"kind": "image", "name": "No file yet"})).unwrap();
    s.run("source.add", &json!({"kind": "audio_input", "name": "Mic"})).unwrap();
    assert!(settle(&mut s, View::Program), "an image without a file publishes nothing, and an audio source draws nothing");
}

#[test]
fn interface_commands_come_back_as_actions() {
    let mut s = headless();
    assert_eq!(s.run("app.settings", &json!({})).unwrap().1, Some(UiAction::OpenSettings));
    assert_eq!(s.run("app.exit", &json!({})).unwrap().1, Some(UiAction::Exit));
    assert_eq!(s.run("scene.add", &json!({})).unwrap().1, None);
}

#[test]
fn unknown_commands_are_errors() {
    let mut s = headless();
    let e = s.run("scene.fly", &json!({})).unwrap_err();
    assert!(matches!(e, StudioError::Engine(_)), "{e}");
}

#[test]
fn recording_needs_the_graphics_device() {
    let mut s = headless();
    let e = s.run("record.start", &json!({})).unwrap_err();
    assert!(matches!(e, StudioError::Output(_)), "{e}");
    assert!(s.output(OutputKind::Recording).is_none());
    assert_eq!(s.state_json()["recording"]["active"], json!(false));
}

#[test]
fn streaming_needs_a_server() {
    let Some(gpu) = shared_gpu() else {
        eprintln!("no graphics device; skipped");
        return;
    };
    let mut s = studio_with(Some(gpu), SceneCollection::new("Test", "Scene"), Profile::default());
    let e = s.run("stream.start", &json!({})).unwrap_err();
    assert!(e.to_string().contains("server"), "{e}");
}

#[test]
fn the_profile_changes_the_canvas() {
    let mut s = headless();
    let mut p = s.profile().clone();
    p.video.base_width = 1280;
    p.video.base_height = 720;
    s.set_profile(p).unwrap();
    assert_eq!(s.canvas(), (1280, 720));
    assert_eq!(s.state_json()["canvas"]["width"], json!(1280));
}

#[test]
fn volume_changes_are_undoable_and_reach_the_collection() {
    let mut s = headless();
    s.run("source.add", &json!({"kind": "color", "name": "Back"})).unwrap();
    s.run("source.volume", &json!({"source": "Back", "gain": 0.5})).unwrap();
    let gain = |s: &Studio| s.collection().sources.iter().find(|x| x.name == "Back").map(|x| x.audio.gain);
    assert_eq!(gain(&s), Some(0.5));
    s.run("edit.undo", &json!({})).unwrap();
    assert_eq!(gain(&s), Some(1.0));
}

fn small_profile() -> Profile {
    let mut profile = Profile::default();
    profile.video.base_width = 320;
    profile.video.base_height = 180;
    profile.video.output_width = 320;
    profile.video.output_height = 180;
    profile
}

fn center(c: &Capture) -> [u8; 4] {
    let i = ((c.height / 2 * c.width + c.width / 2) * 4) as usize;
    [c.rgba[i], c.rgba[i + 1], c.rgba[i + 2], c.rgba[i + 3]]
}

#[test]
fn captures_the_program_picture() {
    let Some(gpu) = shared_gpu() else {
        eprintln!("no graphics device; skipped");
        return;
    };
    let mut s = studio_with(Some(gpu), SceneCollection::new("Test", "Scene"), small_profile());
    s.run("source.add", &json!({"kind": "color", "name": "Red", "settings": {"color": "#ff0000", "width": 320, "height": 180}})).unwrap();
    s.tick();
    // The source thread publishes its first content shortly after it starts.
    let mut last = None;
    for _ in 0..200 {
        let shot = s.capture(View::Program, Duration::from_secs(5)).unwrap();
        assert_eq!((shot.width, shot.height, shot.rgba.len()), (320, 180, 320 * 180 * 4));
        let px = center(&shot);
        last = Some(px);
        if px == [255, 0, 0, 255] {
            break;
        }
    }
    assert_eq!(last, Some([255, 0, 0, 255]));
}

#[test]
fn scope_copies_keep_the_canvas_shape_and_stay_even() {
    assert_eq!(scope_size((1920, 1080), 480, 270), (480, 270));
    assert_eq!(scope_size((1080, 1920), 480, 270), (150, 270));
    assert_eq!(scope_size((1000, 1000), 480, 270), (270, 270));
    assert_eq!(scope_size((321, 181), 480, 270), (478, 270));
    assert_eq!(scope_size((u32::MAX, 1), 480, 270), (480, 2));
    assert_eq!(scope_size((0, 0), 0, 0), (2, 2));
}

#[test]
fn the_scope_feed_receives_small_copies_of_the_program() {
    let mut s = headless();
    let sink = Arc::new(Fifo::new(4));
    s.set_scope_feed(Some(ScopeFeed { sink: Arc::clone(&sink), max_width: 48, max_height: 27 }));
    let Some(gpu) = shared_gpu() else {
        eprintln!("no graphics device; skipped");
        return;
    };
    let mut s = studio_with(Some(gpu), SceneCollection::new("Test", "Scene"), small_profile());
    s.set_scope_feed(Some(ScopeFeed { sink: Arc::clone(&sink), max_width: 48, max_height: 27 }));
    s.tick();
    // Each capture waits for a compositor frame, which also sends the scopes their copy.
    let mut got = None;
    for _ in 0..50 {
        s.capture(View::Program, Duration::from_secs(5)).unwrap();
        got = sink.pop();
        if got.is_some() {
            break;
        }
    }
    let f = got.expect("a scope frame arrived");
    assert_eq!((f.width, f.height), (48, 26), "320 × 180 fitted into 48 × 27 and made even");
    s.set_scope_feed(None);
}

#[test]
fn a_settled_program_shows_the_new_scene_and_its_source() {
    let Some(gpu) = shared_gpu() else {
        eprintln!("no graphics device; skipped");
        return;
    };
    let mut s = studio_with(Some(gpu), SceneCollection::new("Test", "Scene"), small_profile());
    s.run("transition.duration", &json!({"ms": 1000})).unwrap();
    s.run("scene.add", &json!({"name": "Second"})).unwrap();
    s.run("source.add", &json!({"kind": "color", "scene": "Second", "name": "Red", "settings": {"color": "#ff0000", "width": 320, "height": 180}}))
        .unwrap();
    assert!(settle(&mut s, View::Program));
    let shot = s.capture(View::Program, Duration::from_secs(5)).unwrap();
    assert_eq!(center(&shot), [255, 0, 0, 255], "the fade has ended and the new source is drawn");
}

#[test]
fn captures_the_preview_in_studio_mode_only() {
    let Some(gpu) = shared_gpu() else {
        eprintln!("no graphics device; skipped");
        return;
    };
    let mut s = studio_with(Some(gpu), SceneCollection::new("Test", "Scene"), small_profile());
    assert_eq!(s.capture(View::Preview, Duration::from_secs(5)), Err(StudioError::NoPreview));
    s.run("studio.toggle", &json!({})).unwrap();
    let shot = s.capture(View::Preview, Duration::from_secs(5)).unwrap();
    assert_eq!((shot.width, shot.height), (320, 180));
}

#[test]
fn capturing_needs_the_graphics_device() {
    let s = headless();
    assert_eq!(s.capture(View::Program, Duration::from_millis(10)), Err(StudioError::NoGpu));
    assert_eq!(s.capture(View::Preview, Duration::from_millis(10)), Err(StudioError::NoGpu));
}

#[test]
fn png_scales_down_keeping_the_aspect() {
    let shot = Capture { width: 1920, height: 1080, rgba: vec![200; 1920 * 1080 * 4] };
    let bytes = capture::png(&shot, 960).unwrap();
    let img = image::load_from_memory(&bytes).unwrap();
    assert_eq!((img.width(), img.height()), (960, 540));
    let full = image::load_from_memory(&capture::png(&shot, 0).unwrap()).unwrap();
    assert_eq!((full.width(), full.height()), (1920, 1080));
    assert_eq!(capture::scaled_size(8192, 4096, 0), (4096, 2048), "capped at 4096");
    assert_eq!(capture::scaled_size(640, 360, 960), (640, 360), "never scaled up");
}

#[test]
fn png_refuses_a_buffer_that_does_not_match_its_size() {
    let short = Capture { width: 4, height: 4, rgba: vec![0; 4 * 4 * 4 - 1] };
    assert!(matches!(capture::png(&short, 0), Err(StudioError::Image(_))));
    let long = Capture { width: 4, height: 4, rgba: vec![0; 4 * 4 * 4 + 4] };
    assert!(matches!(capture::png(&long, 0), Err(StudioError::Image(_))));
    let empty = Capture { width: 0, height: 4, rgba: vec![] };
    assert!(matches!(capture::png(&empty, 0), Err(StudioError::Image(_))));
    let huge = Capture { width: u32::MAX, height: u32::MAX, rgba: vec![0; 16] };
    assert!(matches!(capture::png(&huge, 0), Err(StudioError::Image(_))));
}

/// Records one second of a color source to a file through ffmpeg, when a graphics device and
/// ffmpeg are present.
#[test]
fn records_a_file() {
    let Some(gpu) = shared_gpu() else {
        eprintln!("no graphics device; skipped");
        return;
    };
    if studiocraft_platform::ffmpeg::locate("").is_err() {
        eprintln!("no ffmpeg; skipped");
        return;
    }
    let dir = std::env::temp_dir().join(format!("studiocraft-record-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut profile = Profile::default();
    profile.video.base_width = 320;
    profile.video.base_height = 180;
    profile.video.output_width = 320;
    profile.video.output_height = 180;
    profile.output.folder = dir.to_string_lossy().into_owned();
    let mut s = studio_with(Some(gpu), SceneCollection::new("Test", "Scene"), profile);
    s.run("source.add", &json!({"kind": "color", "name": "Back", "settings": {"color": "#3366cc", "width": 320, "height": 180}})).unwrap();
    s.run("record.start", &json!({})).unwrap();
    let state = s.output(OutputKind::Recording).unwrap();
    assert!(state.active);
    assert!(state.target.ends_with(".mkv"), "{}", state.target);
    std::thread::sleep(Duration::from_millis(1500));
    s.tick();
    s.run("record.stop", &json!({})).unwrap();
    let until = Instant::now() + Duration::from_secs(30);
    let mut saved = None;
    while Instant::now() < until && saved.is_none() {
        s.tick();
        saved = s.take_notices().into_iter().find(|n| n.text().starts_with("Recording saved"));
        std::thread::sleep(Duration::from_millis(50));
    }
    let saved = saved.expect("the recording finishes");
    assert!(!saved.error);
    let size = std::fs::metadata(&state.target).map(|m| m.len()).unwrap_or(0);
    assert!(size > 1000, "{} is {size} bytes", state.target);
    drop(s);
    let _ = std::fs::remove_dir_all(&dir);
}

mod monitoring {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use studiocraft_platform::PlatformError;
    use studiocraft_platform::audio::{AudioSink, Fill, Format, OnError, Playing};

    use super::*;

    /// A stand-in for the system's output devices: it lists `default` and `Headphones`, and
    /// refuses to open while `refuse` is set.
    #[derive(Default)]
    struct FakeSink {
        opened: Mutex<Vec<String>>,
        callbacks: Mutex<Vec<(Fill, OnError)>>,
        refuse: AtomicBool,
        live: Arc<AtomicUsize>,
    }

    struct FakePlaying(Arc<AtomicUsize>);

    impl Playing for FakePlaying {
        fn format(&self) -> Format {
            Format { rate: 44_100, channels: 2 }
        }
    }

    impl Drop for FakePlaying {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl AudioSink for FakeSink {
        fn play(&self, id: &str, fill: Fill, on_error: OnError) -> Result<Box<dyn Playing>, PlatformError> {
            if self.refuse.load(Ordering::SeqCst) {
                return Err(PlatformError::Failed("the device is busy".into()));
            }
            if !["default", "Headphones"].contains(&id) {
                return Err(PlatformError::NotFound(format!("the audio device \u{201c}{id}\u{201d}")));
            }
            self.opened.lock().unwrap().push(id.to_owned());
            self.callbacks.lock().unwrap().push((fill, on_error));
            self.live.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakePlaying(Arc::clone(&self.live))))
        }
    }

    impl FakeSink {
        fn opened(&self) -> Vec<String> {
            self.opened.lock().unwrap().clone()
        }

        fn live(&self) -> usize {
            self.live.load(Ordering::SeqCst)
        }
    }

    /// A studio with the default collection (Desktop audio and Microphone), playing on a fake.
    fn studio() -> (Studio, Arc<FakeSink>) {
        let mut s = studio_with(None, default_collection("Test"), Profile::default());
        let sink = Arc::new(FakeSink::default());
        s.set_audio_sink(Arc::clone(&sink) as Arc<dyn AudioSink>);
        s.take_notices();
        (s, sink)
    }

    fn monitor(s: &mut Studio, source: &str, mode: &str) {
        s.run("source.monitor", &json!({"source": source, "mode": mode})).unwrap();
    }

    fn errors(s: &mut Studio) -> Vec<String> {
        s.take_notices().into_iter().filter(|n| n.error && n.template.starts_with("Audio monitoring")).map(|n| n.text()).collect()
    }

    #[test]
    fn the_device_is_open_while_a_source_is_monitored() {
        let (mut s, sink) = studio();
        assert!(sink.opened().is_empty() && s.monitoring().is_none(), "nothing is monitored at first");
        monitor(&mut s, "Microphone", "monitor_only");
        assert_eq!(sink.opened(), ["default"]);
        assert_eq!(s.monitoring(), Some(Format { rate: 44_100, channels: 2 }));
        let state = s.state_json();
        assert_eq!(state["monitoring"], json!({"device": "default", "playing": true, "rate": 44_100, "channels": 2}), "{state}");
        monitor(&mut s, "Desktop audio", "monitor_and_output");
        monitor(&mut s, "Microphone", "off");
        assert_eq!((sink.opened().len(), sink.live()), (1, 1), "the device stays open while one source is monitored");
        monitor(&mut s, "Desktop audio", "off");
        assert_eq!(sink.live(), 0, "the last source off closes the device");
        assert_eq!(s.state_json()["monitoring"], json!({"device": "default", "playing": false}));
        s.run("edit.undo", &json!({})).unwrap();
        assert_eq!((sink.opened().len(), sink.live()), (2, 1), "undo opens it again");
        assert!(errors(&mut s).is_empty());
    }

    #[test]
    fn a_new_monitoring_device_takes_over_and_an_unknown_one_is_reported() {
        let (mut s, sink) = studio();
        monitor(&mut s, "Microphone", "monitor_and_output");
        let mut p = s.profile().clone();
        p.audio.monitoring_device = "Headphones".into();
        s.set_profile(p.clone()).unwrap();
        assert_eq!(sink.opened(), ["default", "Headphones"]);
        assert_eq!(sink.live(), 1, "the default device was closed");
        p.audio.monitoring_device = "Gone".into();
        s.set_profile(p.clone()).unwrap();
        assert_eq!(sink.live(), 0);
        let e = errors(&mut s);
        assert!(matches!(e.as_slice(), [m] if m.contains("Gone")), "{e:?}");
        s.run("source.volume", &json!({"source": "Microphone", "gain": 0.5})).unwrap();
        assert!(errors(&mut s).is_empty(), "the missing device is not tried on every change");
        assert_eq!(s.state_json()["monitoring"]["playing"], json!(false));
        p.audio.monitoring_device = "default".into();
        s.set_profile(p).unwrap();
        assert_eq!(sink.live(), 1);
    }

    #[test]
    fn a_device_that_fails_to_open_is_reported_and_tried_again_after_a_change() {
        let (mut s, sink) = studio();
        sink.refuse.store(true, Ordering::SeqCst);
        monitor(&mut s, "Microphone", "monitor_only");
        assert_eq!(errors(&mut s), ["Audio monitoring could not start: the device is busy"]);
        assert!(s.monitoring().is_none());
        sink.refuse.store(false, Ordering::SeqCst);
        s.run("source.mute", &json!({"source": "Microphone", "muted": true})).unwrap();
        assert_eq!(sink.live(), 0, "a failed device is not opened again on an unrelated change");
        monitor(&mut s, "Desktop audio", "monitor_only");
        assert_eq!(sink.live(), 0, "nor when another source is monitored as well");
        monitor(&mut s, "Microphone", "off");
        monitor(&mut s, "Desktop audio", "off");
        monitor(&mut s, "Microphone", "monitor_only");
        assert_eq!(sink.live(), 1, "turning monitoring off and on tries again");
    }

    #[test]
    fn a_device_that_fails_while_playing_stops_monitoring_and_the_studio_runs_on() {
        let (mut s, sink) = studio();
        monitor(&mut s, "Microphone", "monitor_only");
        {
            let mut callbacks = sink.callbacks.lock().unwrap();
            let (fill, on_error) = callbacks.last_mut().unwrap();
            let mut out = vec![9.0f32; 512];
            fill(&mut out, Format { rate: 44_100, channels: 2 });
            assert!(out.iter().all(|v| *v == 0.0), "the device plays silence before the mix arrives");
            on_error("the device was unplugged".into());
        }
        s.tick();
        assert_eq!(errors(&mut s), ["Audio monitoring stopped: the device was unplugged"]);
        assert_eq!(sink.live(), 0);
        s.tick();
        s.run("source.volume", &json!({"source": "Microphone", "gain": 0.25})).unwrap();
        assert_eq!((sink.opened().len(), sink.live()), (1, 0), "the failed device is not opened again on its own");
        assert_eq!(s.collection().source_by_name("Microphone").map(|m| m.audio.monitor), Some(studiocraft_model::MonitorMode::MonitorOnly));
        let mut p = s.profile().clone();
        p.audio.monitoring_device = "Headphones".into();
        s.set_profile(p).unwrap();
        assert_eq!(sink.live(), 1, "another device opens");
    }
}

#[test]
fn collections_are_saved_to_and_opened_from_a_path() {
    let dir = std::env::temp_dir().join(format!("studiocraft-collection-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("show.json");
    let mut s = headless();
    s.run("scene.add", &json!({"name": "Second"})).unwrap();
    assert!(s.engine().is_dirty());
    s.save_collection(&path).unwrap();
    assert!(!s.engine().is_dirty(), "saving marks the collection saved");

    let mut other = headless();
    other.run("scene.add", &json!({"name": "Undo me"})).unwrap();
    other.open_collection(&path).unwrap();
    assert!(other.collection().scene_by_name("Second").is_some());
    assert!(other.collection().scene_by_name("Undo me").is_none());
    assert!(other.run("edit.undo", &json!({})).is_ok());
    assert!(other.collection().scene_by_name("Second").is_some(), "the history starts empty");

    let before = other.collection().clone();
    assert!(matches!(other.open_collection(&dir.join("absent.json")), Err(FileError::Io { .. })));
    assert_eq!(other.collection(), &before, "a failed open keeps the collection");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A device on wgpu's noop backend, which opens without a GPU.
fn noop_gpu() -> Gpu {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
    desc.backends = wgpu::Backends::NOOP;
    desc.backend_options.noop = wgpu::NoopBackendOptions { enable: true, ..Default::default() };
    Gpu::open_on(wgpu::Instance::new(desc), &studiocraft_render::device::Preference::Software).unwrap().0
}

#[test]
fn a_lost_device_the_studio_opened_is_opened_again() {
    let gpu = noop_gpu();
    let mut s = studio_with(Some(gpu.clone()), SceneCollection::new("Test", "Scene"), Profile::default());
    s.take_notices();
    gpu.device.destroy();
    s.tick();
    let notices = s.take_notices();
    let templates: Vec<&str> = notices.iter().map(|n| n.template).collect();
    assert!(templates.contains(&"The graphics device stopped working: {error}"), "{templates:?}");
    assert!(templates.contains(&"Drawing again on {adapter}"), "{templates:?}");
    let now = s.gpu().unwrap();
    assert!(!now.is_lost(), "a new device draws");
    assert!(gpu.is_lost());
    assert!(s.gpu_lost().is_none());
}

#[test]
fn a_lost_window_device_waits_for_the_window() {
    // A device the studio did not open, like the window's.
    let opened = noop_gpu();
    let gpu = Gpu::new(opened.device.clone(), opened.queue.clone());
    assert!(!gpu.can_reopen());
    let mut s = studio_with(Some(gpu.clone()), SceneCollection::new("Test", "Scene"), Profile::default());
    s.take_notices();
    gpu.device.destroy();
    s.tick();
    assert!(s.gpu_lost().is_some(), "the loss is reported");
    assert!(s.views().is_none(), "nothing is drawn with the lost device");
    let stopped = s.take_notices().iter().filter(|n| n.template == "The graphics device stopped working: {error}").count();
    assert_eq!(stopped, 1);
    s.tick();
    assert!(s.take_notices().is_empty(), "the loss is reported once");
    // a video change does not start drawing on the lost device
    let mut p = Profile::default();
    p.video.base_width = 640;
    p.video.base_height = 360;
    assert!(s.set_profile(p).is_ok());
    assert!(s.views().is_none(), "no compositor is made on the lost device");
    s.set_gpu(Some(noop_gpu()));
    assert!(s.gpu_lost().is_none(), "a new device replaces it");
}

#[test]
fn the_outputs_stop_when_the_device_is_lost() {
    // the window's device, and one the studio opened and opens again
    for window in [true, false] {
        let opened = noop_gpu();
        let gpu = if window { Gpu::new(opened.device.clone(), opened.queue.clone()) } else { opened };
        let mut s = studio_with(Some(gpu.clone()), SceneCollection::new("Test", "Scene"), Profile::default());
        s.start_output(OutputKind::Replay).unwrap();
        assert!(s.output(OutputKind::Replay).is_some_and(|o| o.active));
        gpu.device.destroy();
        s.tick();
        assert!(s.output(OutputKind::Replay).is_none_or(|o| !o.active), "no output runs on without a picture (window {window})");
    }
}

#[test]
fn a_vaapi_encoder_chosen_by_name_is_tested_for_its_render_node() {
    let dir = std::env::temp_dir().join(format!("studiocraft-vaapi-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake = dir.join("ffmpeg-not-a-program");
    std::fs::write(&fake, b"not a program").unwrap();
    let mut p = Profile::default();
    p.output.ffmpeg_path = fake.to_string_lossy().into_owned();
    p.output.video_encoder = "vaapi_h264".into();
    p.output.folder = dir.to_string_lossy().into_owned();
    let mut s = studio_with(Some(noop_gpu()), SceneCollection::new("Test", "Scene"), p);
    s.start_output(OutputKind::Recording).unwrap();
    assert!(s.testing_hardware(OutputKind::Recording));
    s.stop_output(OutputKind::Recording);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hardware_encoder_tests_run_off_the_calling_thread_and_a_second_toggle_cancels_them() {
    // A file that is not a program: every test finds no ffmpeg at once.
    let dir = std::env::temp_dir().join(format!("studiocraft-hwenc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let fake = dir.join("ffmpeg-not-a-program");
    std::fs::write(&fake, b"not a program").unwrap();
    let mut p = Profile::default();
    p.output.ffmpeg_path = fake.to_string_lossy().into_owned();
    p.output.video_encoder = "hardware".into();
    p.output.folder = dir.to_string_lossy().into_owned();
    let mut s = studio_with(Some(noop_gpu()), SceneCollection::new("Test", "Scene"), p);
    s.start_output(OutputKind::Recording).unwrap();
    assert!(s.testing_hardware(OutputKind::Recording), "the start returns while the tests run");
    let waiting = s.output(OutputKind::Recording).unwrap();
    assert!(!waiting.active && waiting.status == studiocraft_plugin::OutputStatus::Starting, "the output bar shows Starting");
    while s.testing_hardware(OutputKind::Recording) {
        s.tick();
        std::thread::yield_now();
    }
    let notices = s.take_notices();
    assert!(notices.iter().any(|n| n.template == "No hardware encoder passed its test; using {encoder}"), "{notices:?}");
    s.stop_output(OutputKind::Recording);
    s.take_notices();
    s.toggle_output(OutputKind::Recording).unwrap();
    assert!(s.testing_hardware(OutputKind::Recording));
    s.toggle_output(OutputKind::Recording).unwrap();
    assert!(!s.testing_hardware(OutputKind::Recording), "a second toggle cancels the tests");
    s.tick();
    assert!(s.output(OutputKind::Recording).is_none_or(|o| !o.active), "a canceled start records nothing");
    assert!(s.take_notices().iter().all(|n| n.template != "No hardware encoder passed its test; using {encoder}"));
    // A setting changed while the tests run: the result for the old setting is not used.
    s.toggle_output(OutputKind::Recording).unwrap();
    let mut software = s.profile().clone();
    software.output.hardware_encoder = "software".into();
    s.set_profile(software).unwrap();
    while s.testing_hardware(OutputKind::Recording) {
        s.tick();
        std::thread::yield_now();
    }
    let notices = s.take_notices();
    assert!(notices.iter().all(|n| n.template != "No hardware encoder passed its test; using {encoder}"), "{notices:?}");
    s.stop_output(OutputKind::Recording);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn without_ffmpeg_an_mkv_recording_and_a_stream_test_the_hardware_encoders() {
    let dir = std::env::temp_dir().join(format!("studiocraft-no-ffmpeg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut p = Profile::default();
    p.output.ffmpeg_path = dir.join("ffmpeg-missing").to_string_lossy().into_owned();
    p.output.video_encoder = "hardware".into();
    p.output.format = studiocraft_model::RecordingFormat::Mkv;
    p.output.folder = dir.to_string_lossy().into_owned();
    p.stream.video_encoder = "hardware".into();
    p.stream.primary_mut().server = "rtmp://127.0.0.1/live".into();
    let mut s = studio_with(Some(noop_gpu()), SceneCollection::new("Test", "Scene"), p.clone());
    for kind in [OutputKind::Recording, OutputKind::Streaming] {
        s.start_output(kind).unwrap();
        assert!(s.testing_hardware(kind), "{kind:?}: NVENC, Media Foundation, VideoToolbox and VA-API can still run in this process");
        while s.testing_hardware(kind) {
            s.tick();
            std::thread::yield_now();
        }
        s.stop_output(kind);
    }
    // A VA-API encoder chosen by name is tested in the process too; another named encoder needs
    // ffmpeg.
    for (named, tests) in [("vaapi_hevc", true), ("nvenc_h264", false)] {
        let mut named_p = p.clone();
        named_p.output.video_encoder = named.into();
        let mut s = studio_with(Some(noop_gpu()), SceneCollection::new("Test", "Scene"), named_p);
        match s.start_output(OutputKind::Recording) {
            Ok(()) => assert!(tests && s.testing_hardware(OutputKind::Recording), "{named}"),
            Err(e) => assert!(!tests && e.to_string().contains("ffmpeg"), "{named}: {e}"),
        }
        while s.testing_hardware(OutputKind::Recording) {
            s.tick();
            std::thread::yield_now();
        }
        s.stop_output(OutputKind::Recording);
    }
    // With an ffmpeg program present, a stream still neither looks for it nor runs it.
    let present = dir.join("ffmpeg-present");
    std::fs::write(&present, b"").unwrap();
    let mut with_ffmpeg = p.clone();
    with_ffmpeg.output.ffmpeg_path = present.to_string_lossy().into_owned();
    let mut s = studio_with(Some(noop_gpu()), SceneCollection::new("Test", "Scene"), with_ffmpeg);
    s.start_output(OutputKind::Streaming).unwrap();
    while s.testing_hardware(OutputKind::Streaming) {
        s.tick();
        std::thread::yield_now();
    }
    s.stop_output(OutputKind::Streaming);
    assert_eq!(s.ffmpeg_lists_read(), 0, "a stream ran ffmpeg");
    p.output.format = studiocraft_model::RecordingFormat::Mp4;
    let mut s = studio_with(Some(noop_gpu()), SceneCollection::new("Test", "Scene"), p);
    assert!(s.start_output(OutputKind::Recording).is_err(), "an MP4 recording runs in ffmpeg");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn graphics_notices_name_the_adapter_and_the_count() {
    use studiocraft_render::device::Notice as N;
    let n = gpu_notice(&N::SoftwareAfterLosses(3));
    assert_eq!(n.text(), "The graphics device stopped working 3 times; drawing in software");
    let failed = gpu_notice(&N::DeviceFailed { adapter: "GPU".into(), error: "out of memory".into() });
    assert_eq!(failed.text(), "The graphics adapter GPU could not be opened: out of memory");
}

mod audio_tracks {
    use studiocraft_audio_tracks::journal::{self, Line};
    use studiocraft_model::AudioTracksWhen;

    use super::*;

    fn studio(name: &str, enabled: bool) -> (Studio, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("studiocraft-tracks-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut p = Profile::default();
        p.output.folder = dir.to_string_lossy().into_owned();
        p.audio_tracks.enabled = enabled;
        p.audio_tracks.when = AudioTracksWhen::Streaming;
        p.audio_tracks.buffer_mib = 16;
        let mut s = studio_with(None, SceneCollection::new("Test", "Scene"), p);
        s.run("source.add", &json!({"kind": "audio_input", "name": "Mic"})).unwrap();
        s.run("source.add", &json!({"kind": "audio_output", "name": "Desk"})).unwrap();
        s.run("source.monitor", &json!({"source": "Desk", "mode": "monitor_only"})).unwrap();
        (s, dir)
    }

    fn sources(s: &Studio) -> Vec<SourceId> {
        s.audio_tracks.recorded.clone()
    }

    /// Tick until the running take is ready. Bounded by a count of ticks, so a fault fails the
    /// test instead of hanging it.
    fn armed(s: &mut Studio) {
        let mut spins = 0u64;
        while !s.audio_tracks_take().is_some_and(|t| t.status().ready()) {
            spins += 1;
            assert!(spins < 50_000_000, "the take never became ready");
            s.tick();
            std::thread::yield_now();
        }
    }

    #[test]
    fn with_the_setting_off_nothing_is_made() {
        let (mut s, dir) = studio("off", false);
        s.test_streaming = true;
        s.tick();
        s.run("source.set_record_track", &json!({"source": "Mic", "on": true})).unwrap();
        s.tick();
        assert!(s.audio_tracks_idle(), "no take, tap, thread or repair");
        assert_eq!(s.mixer.taps_applied(), 0);
        assert!(!dir.exists(), "no folder is made");
        assert_eq!(s.state_json()["audio_tracks"]["active"], json!(false));
    }

    #[test]
    fn a_take_records_at_most_64_tracks() {
        let (mut s, dir) = studio("many", true);
        for n in 0..70 {
            s.run("source.add", &json!({"kind": "audio_input", "name": format!("Mic {n}")})).unwrap();
        }
        // Mic and the 70 added sources want tracks; with the program that is 72.
        assert_eq!(s.audio_track_sources(), studiocraft_audio_tracks::MAX_TRACKS - 1);
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        assert_eq!(sources(&s).len(), studiocraft_audio_tracks::MAX_TRACKS - 1);
        assert_eq!(s.audio_tracks.slots.len(), studiocraft_audio_tracks::MAX_TRACKS - 1, "no tap for the sources after the limit");
        assert!(s.audio_tracks_source_limit(), "the sources left out are reported");
        assert_eq!(s.state_json()["audio_tracks"]["source_limit"], json!(true));
        // With 9 fewer sources set to record, every one fits.
        for n in 0..9 {
            s.run("source.set_record_track", &json!({"source": format!("Mic {n}"), "on": false})).unwrap();
        }
        s.tick();
        assert!(!s.audio_tracks_source_limit());
        s.test_streaming = false;
        s.tick();
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_take_records_the_program_and_the_program_sources_and_follows_changes() {
        let (mut s, dir) = studio("on", true);
        s.test_streaming = true;
        s.tick();
        assert_eq!(s.audio_tracks.generation, 1, "the taps go to the mixer with the take's start");
        armed(&mut s);
        assert_eq!(s.audio_tracks.generation, 1, "an unchanged collection sends nothing more");
        assert_eq!(s.state_json()["audio_tracks"]["anchor"], json!("stream"));
        let mic = s.collection().source_by_name("Mic").map(|x| x.id).unwrap();
        let desk = s.collection().source_by_name("Desk").map(|x| x.id).unwrap();
        assert!(s.audio_tracks_take().is_some());
        assert_eq!(sources(&s), [mic], "a monitor-only source is left out until it is set");
        assert!(s.audio_tracks.program.is_some());
        s.run("source.set_record_track", &json!({"source": "Desk", "on": true})).unwrap();
        assert_eq!(sources(&s), [mic, desk]);
        s.run("source.set_record_track", &json!({"source": "Mic", "on": false})).unwrap();
        assert_eq!(sources(&s), [desk]);
        assert_eq!(s.state_json()["audio_tracks"]["state"], json!("recording"));
        // A new write buffer applies from the next take, and the status says which one runs.
        let mut p = s.profile().clone();
        p.audio_tracks.buffer_mib = 32;
        s.set_profile(p).unwrap();
        let status = s.state_json()["audio_tracks"].clone();
        assert_eq!((status["buffer_mib"].clone(), status["take_buffer_mib"].clone()), (json!(32), json!(16)));
        s.test_streaming = false;
        s.tick();
        assert!(s.state_json()["audio_tracks"].get("take_buffer_mib").is_none());
        assert!(s.audio_tracks_take().is_none() && s.audio_tracks.slots.is_empty(), "stop removes the taps at once");
        s.finish_audio_tracks_blocking();
        let take = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).find(|p| p.to_string_lossy().contains("Stream ")).unwrap();
        let lines = journal::read(&take.join("Audio Files")).unwrap();
        assert!(matches!(lines.last(), Some(Line::End { .. })), "{lines:?}");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_collection_during_a_take_gives_its_sources_their_own_files() {
        let (mut s, dir) = studio("replace", true);
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        let mic = s.collection().source_by_name("Mic").map(|x| x.id).unwrap();
        s.load_collection(SceneCollection::new("Other", "Scene"));
        s.run("source.add", &json!({"kind": "audio_input", "name": "Guitar"})).unwrap();
        let guitar = s.collection().source_by_name("Guitar").map(|x| x.id).unwrap();
        assert_eq!(guitar, mic, "the new collection reuses the id");
        s.tick();
        let status = Arc::clone(s.audio_tracks_status().unwrap());
        s.test_streaming = false;
        s.tick();
        s.finish_audio_tracks_blocking();
        assert_eq!(status.tracks(), 3, "the program, Mic and Guitar each have a track");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_full_disk_stops_only_the_audio_tracks_and_removes_their_taps() {
        let (mut s, dir) = studio("full", true);
        s.test_faults.full_after_bytes = Some(0);
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        let status = Arc::clone(s.audio_tracks_status().unwrap());
        // The mixer fills the first chunk; the disk thread's write of it fails. The loop is
        // bounded by a count of yields, so a fault fails the test instead of hanging it.
        let mut spins = 0u64;
        while status.state() != Some(AudioTracksState::Stopped) {
            spins += 1;
            assert!(spins < 200_000_000, "the take never stopped: {:?}", status.state());
            std::thread::yield_now();
        }
        s.tick();
        assert_eq!(status.reason(), Some(AudioTracksStopReason::DiskFull));
        assert!(s.audio_tracks_take().is_none() && s.audio_tracks.slots.is_empty() && s.audio_tracks.program.is_none());
        let generation = s.audio_tracks.generation;
        let notices: Vec<String> = s.take_notices().into_iter().map(|n| n.text()).collect();
        assert!(notices.iter().any(|n| n.starts_with("Audio tracks stopped at ") && n.ends_with(": the disk is full")), "{notices:?}");
        // Its threads end; the stop was reported once and is not reported again.
        let mut spins = 0u64;
        while !s.audio_tracks.finishing.is_empty() {
            spins += 1;
            assert!(spins < 200_000_000, "the take never finished");
            s.tick();
            std::thread::yield_now();
        }
        let again: Vec<String> = s.take_notices().into_iter().map(|n| n.text()).collect();
        assert!(!again.iter().any(|n| n.starts_with("Audio tracks stopped")), "reported twice: {again:?}");
        s.tick();
        assert!(s.audio_tracks_take().is_none(), "no new take while the stream still runs");
        assert_eq!(s.audio_tracks.generation, generation);
        assert_eq!(s.state_json()["audio_tracks"]["reason"], json!("the disk is full"));
        s.test_streaming = false;
        s.tick();
        s.test_faults.full_after_bytes = None;
        s.test_streaming = true;
        s.tick();
        assert!(s.audio_tracks_take().is_some(), "the next stream starts a take again");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_output_that_stops_in_the_tick_that_sees_an_early_stop_lets_the_next_one_record() {
        let (mut s, dir) = studio("full-then-stop", true);
        s.test_faults.full_after_bytes = Some(0);
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        let status = Arc::clone(s.audio_tracks_status().unwrap());
        let mut spins = 0u64;
        while status.state() != Some(AudioTracksState::Stopped) {
            spins += 1;
            assert!(spins < 200_000_000, "the take never stopped: {:?}", status.state());
            std::thread::yield_now();
        }
        // The stream stops before the studio sees the early stop; both are seen in one tick.
        s.test_streaming = false;
        s.tick();
        s.test_faults.full_after_bytes = None;
        s.test_streaming = true;
        s.tick();
        assert!(s.audio_tracks_take().is_some(), "the next stream starts a take");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn settings_out_of_range_are_refused_and_change_nothing() {
        let (mut s, dir) = studio("range", false);
        let mut p = s.profile().clone();
        p.audio_tracks.enabled = true;
        p.audio_tracks.segment_gib = 0;
        let e = s.set_profile(p).unwrap_err().to_string();
        assert!(e.contains("audio_tracks.segment_gib"), "{e}");
        assert!(!s.profile().audio_tracks.enabled, "the settings stay as they were");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_that_fails_while_the_take_finishes_is_reported() {
        let (mut s, dir) = studio("finish-full", true);
        s.test_faults.full_after_bytes = Some(0);
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        let status = Arc::clone(s.audio_tracks_status().unwrap());
        // Some audio reaches the collect thread, less than a chunk, so nothing is written yet.
        let mut spins = 0u64;
        while status.seconds() == 0.0 {
            spins += 1;
            assert!(spins < 200_000_000, "no audio reached the take");
            std::thread::yield_now();
        }
        s.test_streaming = false;
        s.tick();
        let early: Vec<String> = s.take_notices().into_iter().map(|n| n.text()).collect();
        assert!(!early.iter().any(|n| n.starts_with("Audio tracks stopped")), "stopping is not an error: {early:?}");
        // The last chunk is written while the take finishes, and that write fails.
        let mut spins = 0u64;
        while !s.audio_tracks.finishing.is_empty() {
            spins += 1;
            assert!(spins < 200_000_000, "the take never finished");
            s.tick();
            std::thread::yield_now();
        }
        assert_eq!(status.state(), Some(AudioTracksState::Stopped));
        let notices: Vec<String> = s.take_notices().into_iter().map(|n| n.text()).collect();
        assert!(notices.iter().any(|n| n.starts_with("Audio tracks stopped at ") && n.ends_with(": the disk is full")), "{notices:?}");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_take_follows_the_recording_when_it_starts_and_is_thrown_away_when_it_fails() {
        let (mut s, dir) = studio("anchor", true);
        let mut p = s.profile().clone();
        p.audio_tracks.when = AudioTracksWhen::Either;
        s.set_profile(p).unwrap();
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        assert_eq!(s.state_json()["audio_tracks"]["anchor"], json!("stream"));
        // A recording starts while the stream runs: the stream's take ends and one beside the
        // recording starts before the recording's first sample.
        let rec = dir.join("2026-11-18 21-00-00.mkv");
        let now = s.clock.now();
        s.audio_tracks_output_starting(OutputKind::Recording, now, Some(&rec));
        assert_eq!(s.state_json()["audio_tracks"]["anchor"], json!("recording"));
        assert_eq!(s.audio_tracks.finishing.len(), 1);
        // The recording does not start: its take, folder and files are removed, and the stream's
        // take continues at once from where it ended.
        s.audio_tracks_output_failed(OutputKind::Recording);
        assert!(s.audio_tracks_take().is_some(), "the stream has a take again");
        assert_eq!(s.state_json()["audio_tracks"]["anchor"], json!("stream"));
        s.test_streaming = false;
        s.tick();
        s.finish_audio_tracks_blocking();
        assert!(!dir.join("2026-11-18 21-00-00 Audio").exists(), "the take of a recording that never started leaves nothing");
        let streams: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("Stream ")))
            .collect();
        assert_eq!(streams.len(), 2, "the stream's take before the recording and the one after it");
        // The stream started at tick 0 in this test, so the second take's time reference is
        // where the first one ended.
        let mut offsets: Vec<u64> = streams
            .iter()
            .filter_map(|d| journal::read(&d.join("Audio Files")).ok())
            .filter_map(|lines| {
                lines.into_iter().find_map(|l| match l {
                    Line::Start { offset_frames, .. } => Some(offset_frames),
                    _ => None,
                })
            })
            .collect();
        offsets.sort_unstable();
        assert_eq!(offsets.len(), 2);
        assert_eq!(offsets[1], u64::try_from(now.0 / studiocraft_audio_tracks::collect::FRAME_TICKS).unwrap());
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audio_tracks_status_and_repair_are_commands() {
        let (mut s, dir) = studio("cmd", false);
        let (v, _) = s.run("audio_tracks.status", &json!({})).unwrap();
        assert_eq!((v["enabled"].clone(), v["buffer_mib"].clone()), (json!(false), json!(16)));
        std::fs::create_dir_all(&dir).unwrap();
        // The repair runs on its own thread and the command returns at once.
        let (v, _) = s.run("audio_tracks.repair", &json!({"folder": dir.to_string_lossy()})).unwrap();
        assert_eq!(v["started"], json!(true));
        assert_eq!(s.state_json()["audio_tracks"]["repairing"], json!(true));
        assert_eq!(s.audio_tracks_repair_result(), Some(json!({"repaired": [], "failed": []})));
        assert_eq!(s.state_json()["audio_tracks"]["repairing"], json!(false));
        assert_eq!(s.state_json()["audio_tracks"]["last_repair"], json!({"repaired": [], "failed": []}));
        let missing = s.run("audio_tracks.repair", &json!({"folder": dir.join("missing").to_string_lossy()}));
        assert!(missing.is_err(), "a folder that is not there is an error");
        assert_eq!(s.state_json()["audio_tracks"]["repairing"], json!(false), "and starts no repair");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_repair_reads_only_folders_inside_the_recording_folder() {
        let (mut s, dir) = studio("repair-roots", false);
        let take = dir.join("Show Audio").join("Audio Files");
        std::fs::create_dir_all(&take).unwrap();
        // A sibling whose name starts with the recording folder's is outside it.
        let sibling = std::path::PathBuf::from(format!("{}-other", dir.display()));
        std::fs::create_dir_all(&sibling).unwrap();
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut outside = vec![sibling.clone(), dir.join("Show Audio").join("..").join("..")];
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&sibling, dir.join("link")).unwrap();
            outside.push(dir.join("link"));
        }
        for folder in &outside {
            let r = s.run("audio_tracks.repair", &json!({"folder": folder.to_string_lossy()}));
            let Err(e) = r else { panic!("{} was repaired", folder.display()) };
            assert!(e.to_string().contains("not inside the recording folder"), "{}: {e}", folder.display());
            assert_eq!(s.state_json()["audio_tracks"]["repairing"], json!(false), "{} started a repair", folder.display());
        }
        for folder in [dir.clone(), take, dir.join("Show Audio").join("..")] {
            let (v, _) = s.run("audio_tracks.repair", &json!({"folder": folder.to_string_lossy()})).unwrap();
            assert_eq!(v["started"], json!(true), "{}", folder.display());
            assert_eq!(s.audio_tracks_repair_result(), Some(json!({"repaired": [], "failed": []})));
        }
        // Without the recording folder nothing is repaired.
        let _ = std::fs::remove_dir_all(&dir);
        let r = s.run("audio_tracks.repair", &json!({"folder": sibling.to_string_lossy()}));
        assert!(r.is_err_and(|e| e.to_string().contains("could not be read, so nothing is repaired")));
        let _ = std::fs::remove_dir_all(&sibling);
    }
}

mod captions {
    use super::*;
    use craft_speech::{Options, ProgressFn, SpeechError, Transcriber, Transcript};
    use studiocraft_sources::CaptionBoard;

    struct Quiet;
    impl Transcriber for Quiet {
        fn id(&self) -> String {
            "quiet".into()
        }
        fn transcribe(&self, _: &[f32], _: &Options, _: ProgressFn) -> Result<Transcript, SpeechError> {
            Ok(Transcript::default())
        }
    }

    fn captioned() -> Studio {
        let mut s = headless();
        s.set_caption_board(Arc::new(CaptionBoard::new()));
        s
    }

    #[test]
    fn captions_need_a_downloaded_model_and_valid_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = captioned();
        s.set_models_dir(dir.path().to_path_buf());
        let e = s.run("captions.start", &json!({})).unwrap_err().to_string();
        assert!(e.contains("not downloaded") && e.contains("captions.download"), "{e}");
        let e = s.run("captions.start", &json!({"model": "nope"})).unwrap_err().to_string();
        assert!(e.contains("no speech model `nope`"), "{e}");
        let e = s.run("captions.start", &json!({"model": "whisper-tiny", "language": "English"})).unwrap_err().to_string();
        assert!(e.contains("not a language the speech models recognize"), "{e}");
        let e = s.run("captions.start", &json!({"model": "whisper-tiny", "language": "zz"})).unwrap_err().to_string();
        assert!(e.contains("not a language the speech models recognize"), "{e}");
        let e = s.run("captions.download", &json!({"model": "nope"})).unwrap_err().to_string();
        assert!(e.contains("no speech model `nope`"), "{e}");
        assert_eq!(s.captions_state(), CaptionState::Stopped);
        let status = s.run("captions.status", &json!({})).unwrap().0;
        assert_eq!(status["state"], "stopped");
        assert_eq!(status["models"].as_array().map(Vec::len), Some(craft_speech::models::catalog().len()));
        assert!(status["models"].as_array().unwrap().iter().all(|m| m["downloaded"] == false));
        assert_eq!(s.state_json()["captions"], status);
        assert!(!dir.path().join("whisper-base").exists(), "a failed start downloads nothing");
    }

    #[test]
    fn captions_start_once_and_stop_with_a_notice() {
        let mut s = captioned();
        s.set_caption_loader(Arc::new(|_: &Path, _: &str| Ok(Arc::new(Quiet) as Arc<dyn Transcriber>)));
        let (v, _) = s.run("captions.start", &json!({"model": "whisper-tiny", "language": "de"})).unwrap();
        assert_eq!(v, json!({"model": "whisper-tiny", "language": "de"}));
        assert!(matches!(s.captions_state(), CaptionState::Loading | CaptionState::Running));
        let e = s.run("captions.start", &json!({})).unwrap_err().to_string();
        assert!(e.contains("already running"), "{e}");
        assert_eq!(s.state_json()["captions"]["model"], "whisper-tiny");
        assert_eq!(s.run("captions.toggle", &json!({})).unwrap().0, json!({"running": false}));
        s.finish_captions_blocking();
        assert_eq!(s.captions_state(), CaptionState::Stopped);
        let notices: Vec<&str> = s.take_notices().iter().map(|n| n.template).collect();
        assert!(notices.contains(&"Live captions started with {model}"), "{notices:?}");
        assert!(notices.contains(&"Live captions stopped"), "{notices:?}");
        // Stopped, it starts again.
        s.run("captions.toggle", &json!({})).unwrap();
        assert!(matches!(s.captions_state(), CaptionState::Loading | CaptionState::Running));
    }

    #[test]
    fn a_failed_model_load_is_reported_and_ends_the_run() {
        let mut s = captioned();
        s.set_caption_loader(Arc::new(|_: &Path, _: &str| Err(SpeechError::Model("broken weights".into()))));
        s.run("captions.start", &json!({})).unwrap();
        s.finish_captions_blocking();
        let notices = s.take_notices();
        let failed = notices.iter().find(|n| n.template == "Live captions stopped: {error}");
        assert!(failed.is_some_and(|n| n.error && n.text().contains("broken weights")), "{notices:?}");
        assert!(!notices.iter().any(|n| n.template == "Live captions stopped"), "{notices:?}");
        // The failure stays readable after the run ends, until the next start.
        assert!(matches!(s.captions_state(), CaptionState::Failed(why) if why.contains("broken weights")));
        let (status, _) = s.run("captions.status", &json!({})).unwrap();
        assert!(status["state"].as_str().is_some_and(|t| t.starts_with("failed") && t.contains("broken weights")), "{status}");
        s.set_caption_loader(Arc::new(|_: &Path, _: &str| Err(SpeechError::Model("still broken".into()))));
        s.run("captions.start", &json!({})).unwrap();
        assert!(!matches!(s.captions_state(), CaptionState::Failed(why) if why.contains("broken weights")));
        s.finish_captions_blocking();
    }
}
