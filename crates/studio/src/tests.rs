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
