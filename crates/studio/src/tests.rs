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
fn without_ffmpeg_only_an_mkv_recording_tests_the_hardware_encoders() {
    let dir = std::env::temp_dir().join(format!("studiocraft-no-ffmpeg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut p = Profile::default();
    p.output.ffmpeg_path = dir.join("ffmpeg-missing").to_string_lossy().into_owned();
    p.output.video_encoder = "hardware".into();
    p.output.format = studiocraft_model::RecordingFormat::Mkv;
    p.output.folder = dir.to_string_lossy().into_owned();
    p.stream.video_encoder = "hardware".into();
    p.stream.server = "rtmp://127.0.0.1/live".into();
    let mut s = studio_with(Some(noop_gpu()), SceneCollection::new("Test", "Scene"), p.clone());
    s.start_output(OutputKind::Recording).unwrap();
    assert!(s.testing_hardware(OutputKind::Recording), "NVENC, Media Foundation, VideoToolbox and VA-API can still run in this process");
    while s.testing_hardware(OutputKind::Recording) {
        s.tick();
        std::thread::yield_now();
    }
    s.stop_output(OutputKind::Recording);
    let e = s.start_output(OutputKind::Streaming).unwrap_err().to_string();
    assert!(e.contains("ffmpeg"), "{e}");
    assert!(!s.testing_hardware(OutputKind::Streaming));
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
