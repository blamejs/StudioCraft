use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use serde_json::json;
use studiocraft_model::{Profile, SceneCollection};
use studiocraft_render::Gpu;

use super::*;

fn studio_with(gpu: Option<Gpu>, collection: SceneCollection, profile: Profile) -> Studio {
    let ffmpeg: FfmpegSetting = Arc::new(RwLock::new(String::new()));
    let captions = Arc::new(CaptionBoard::new());
    let registry = Arc::new(builtin_registry(&ffmpeg, &captions).unwrap());
    Studio::new(gpu, collection, profile, registry, ffmpeg, captions)
}

/// One device for every test in this binary; opening many devices at once crashes some drivers.
fn shared_gpu() -> Option<Gpu> {
    static GPU: std::sync::OnceLock<Option<Gpu>> = std::sync::OnceLock::new();
    GPU.get_or_init(|| Gpu::headless().ok()).clone()
}

/// A studio without audio devices or a graphics device.
pub(crate) fn headless() -> Studio {
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
    let reg = builtin_registry(&ffmpeg, &Arc::new(CaptionBoard::new())).unwrap();
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
fn safe_mode_starts_no_source_until_it_is_left() {
    let mut normal = headless();
    normal.run("source.add", &json!({"kind": "color", "name": "Card"})).unwrap();
    let collection = normal.collection().clone();
    let want = collection.sources.len();
    assert!(want > 0);
    drop(normal);

    let ffmpeg: FfmpegSetting = Arc::new(RwLock::new(String::new()));
    let captions = Arc::new(CaptionBoard::new());
    let registry = Arc::new(builtin_registry(&ffmpeg, &captions).unwrap());
    let mut s = Studio::new_in_safe_mode(None, collection, Profile::default(), registry, ffmpeg, captions);
    assert!(s.safe_mode());
    assert_eq!(s.sources.len(), 0, "no source of the opened collection runs");
    // The collection can still be edited, and a new source does not start either.
    s.run("source.add", &json!({"kind": "color", "name": "Second"})).unwrap();
    s.tick();
    assert_eq!(s.sources.len(), 0);
    assert_eq!(s.collection().sources.len(), want + 1);
    // The pause screen's picture or video is a source too.
    assert!(s.run("pause.start", &json!({"countdown_s": 0})).is_err());

    s.leave_safe_mode();
    assert!(!s.safe_mode());
    assert_eq!(s.sources.len(), want + 1, "every source starts when Safe Mode is left");
    // Leaving again changes nothing.
    s.leave_safe_mode();
    assert_eq!(s.sources.len(), want + 1);
    s.run("pause.start", &json!({"countdown_s": 0})).unwrap();
}

#[test]
fn a_cut_switches_at_once() {
    let mut s = headless();
    s.run("transition.set", &json!({"kind": "cut"})).unwrap();
    s.run("scene.add", &json!({"name": "Second"})).unwrap();
    s.run("scene.select", &json!({"scene": "Second"})).unwrap();
    assert!(s.transition_progress().is_none());
}

mod scene_thumbnails {
    use super::*;

    /// The source ids each requested scene picture draws, bottom to top, in slot order.
    fn drawn(s: &Studio) -> Vec<Vec<u64>> {
        s.thumbnail_request().map(|r| r.scenes.iter().map(|d| d.items.iter().map(|i| i.source.0).collect()).collect()).unwrap_or_default()
    }

    #[test]
    fn the_named_scenes_are_drawn_in_order_until_none_is_named() {
        let mut s = headless();
        let (out, _) = s.run("source.add", &json!({"kind": "color", "name": "Card"})).unwrap();
        let card = out["source"].as_u64().unwrap_or_else(|| panic!("{out}"));
        s.run("scene.add", &json!({"name": "Break"})).unwrap();
        s.tick();
        let first = s.collection().scene_by_name("Scene").map(|c| c.id).unwrap();
        let brk = s.collection().scene_by_name("Break").map(|c| c.id).unwrap();
        assert!(s.thumbnail_request().is_none(), "no picture is drawn before one is asked for");
        assert!(s.thumbnail_scenes().is_empty());

        let (out, _) = s.run("scene.thumbnails", &json!({"scenes": ["Break", "Scene"]})).unwrap();
        assert_eq!(out, json!({"scenes": [brk, first], "fps": 2.0}));
        assert_eq!(s.thumbnail_scenes(), [brk, first]);
        assert_eq!(drawn(&s), [vec![], vec![card]]);
        let request = s.thumbnail_request().unwrap();
        assert_eq!((request.fps, request.size), (2.0, (256, 144)));
        assert_eq!(request.keys, [1, 2], "each slot has a key of its own");
        // A reordered list keeps each scene's key, so only the slots matter to the compositor.
        s.run("scene.thumbnails", &json!({"scenes": ["Scene", "Break"]})).unwrap();
        assert_eq!(s.thumbnail_request().map(|r| r.keys), Some(vec![2, 1]));
        s.run("scene.thumbnails", &json!({"scenes": ["Break", "Scene"]})).unwrap();
        assert!(s.thumbnail_views().is_none(), "nothing is drawn without a graphics device");

        // A refused call keeps the last list.
        assert!(s.run("scene.thumbnails", &json!({"scenes": ["Nope"]})).is_err());
        assert!(s.run("scene.thumbnails", &json!({"scenes": [first], "fps": 5})).is_err());
        assert_eq!(s.thumbnail_scenes(), [brk, first]);

        // A scene's change reaches its picture, and the rate is the one asked for.
        s.run("source.add", &json!({"kind": "color", "name": "Logo", "scene": "Break"})).unwrap();
        s.run("scene.thumbnails", &json!({"scenes": ["Break", "Scene"], "fps": 4})).unwrap();
        assert_eq!(drawn(&s)[0].len(), 1);
        assert_eq!(s.thumbnail_request().map(|r| r.fps), Some(4.0));

        // A removed scene leaves the list, and the pictures stop when none is left.
        let undo = s.engine.undo_label();
        s.run("scene.remove", &json!({"scene": "Break"})).unwrap();
        s.tick();
        assert_eq!(s.thumbnail_scenes(), [first]);
        assert_eq!(drawn(&s), [vec![card]]);
        assert_ne!(s.engine.undo_label(), undo);
        let undo = s.engine.undo_label();
        let (out, _) = s.run("scene.thumbnails", &json!({"scenes": []})).unwrap();
        assert_eq!(out, json!({"scenes": [], "fps": null}));
        assert!(s.thumbnail_request().is_none());
        assert_eq!(s.engine.undo_label(), undo, "no undo step");
        s.run("scene.add", &json!({"name": "Only"})).unwrap();
        s.run("scene.thumbnails", &json!({"scenes": ["Only"]})).unwrap();
        s.run("scene.remove", &json!({"scene": "Only"})).unwrap();
        s.tick();
        assert!(s.thumbnail_scenes().is_empty());
        assert!(s.thumbnail_request().is_none());

        // Another collection's scene ids can match these, so its scenes are not drawn.
        s.run("scene.thumbnails", &json!({"scenes": [first]})).unwrap();
        let old_key = s.thumbnail_request().map(|r| r.keys);
        s.load_collection(SceneCollection::new("Other", "Scene"));
        assert!(s.collection().scene(first).is_some(), "the new collection has a scene with the same id");
        assert!(s.thumbnail_scenes().is_empty());
        assert!(s.thumbnail_request().is_none());
        // Asked for again, its slot gets a new key, so the compositor draws it at once.
        s.run("scene.thumbnails", &json!({"scenes": [first]})).unwrap();
        let new_key = s.thumbnail_request().map(|r| r.keys);
        assert!(new_key.is_some() && new_key != old_key, "{old_key:?} {new_key:?}");
    }

    #[test]
    fn the_compositor_hears_of_scene_pictures_only_while_some_are_shown() {
        let mut s = studio_with(Some(noop_gpu()), SceneCollection::new("Test", "Scene"), small_profile());
        let sent = |s: &Studio| s.video.as_ref().map(|v| v.thumbnails_sent.clone());
        let sends = |s: &Studio| s.video.as_ref().map_or(0, |v| v.thumbnail_sends);
        assert!(matches!(sent(&s), Some(None)), "the compositor runs and draws no scene picture");
        // A list view asks for no pictures, so collection changes send the compositor nothing.
        s.run("source.add", &json!({"kind": "color", "name": "Card"})).unwrap();
        s.tick();
        assert!(matches!(sent(&s), Some(None)));
        assert_eq!(sends(&s), 0);

        s.run("scene.thumbnails", &json!({"scenes": ["Scene"]})).unwrap();
        let first = sent(&s).flatten().expect("the pictures are asked for");
        assert_eq!((first.scenes.len(), first.scenes[0].items.len(), first.fps), (1, 1, 2.0));
        assert_eq!(sends(&s), 1);
        // The same list again sends nothing new.
        s.run("scene.thumbnails", &json!({"scenes": ["Scene"]})).unwrap();
        assert!(sent(&s).flatten().is_some_and(|r| Arc::ptr_eq(&r, &first)));
        assert_eq!(sends(&s), 1);
        // A change to a drawn scene sends the scene as it is now.
        s.run("source.add", &json!({"kind": "color", "name": "Logo"})).unwrap();
        s.tick();
        assert!(sent(&s).flatten().is_some_and(|r| r.scenes[0].items.len() == 2));
        let before = sends(&s);
        s.run("scene.thumbnails", &json!({"scenes": []})).unwrap();
        assert!(matches!(sent(&s), Some(None)), "an empty list stops the pictures");
        assert_eq!(sends(&s), before + 1, "the stop is sent once");
        assert!(s.thumbnail_views().is_none());
        // After the stop, changes send nothing again.
        s.run("source.add", &json!({"kind": "color", "name": "Third"})).unwrap();
        s.tick();
        assert_eq!(sends(&s), before + 1);

        // Textures with fewer slots than the list has scenes are withheld until new ones arrive.
        let mut comp = Compositor::new(noop_gpu(), 32, 18).unwrap();
        let one_slot = ThumbnailRequest { scenes: vec![studiocraft_render::SceneDesc::default()], ..ThumbnailRequest::default() };
        comp.set_thumbnails(Some(Arc::new(one_slot)));
        let one = comp.thumbnail_views().expect("the request makes one slot");
        assert_eq!(one.slots.len(), 1);
        s.run("scene.thumbnails", &json!({"scenes": ["Scene"]})).unwrap();
        if let Some(v) = &mut s.video {
            v.thumbnail_views = Some(Arc::clone(&one));
        }
        assert!(s.thumbnail_views().is_some_and(|v| Arc::ptr_eq(&v, &one)));
        s.run("scene.add", &json!({"name": "Two"})).unwrap();
        s.run("scene.thumbnails", &json!({"scenes": ["Scene", "Two"]})).unwrap();
        assert!(s.thumbnail_views().is_none(), "one slot for two scenes");
        s.run("scene.thumbnails", &json!({"scenes": ["Two"]})).unwrap();
        assert!(s.thumbnail_views().is_some());
    }
}

#[test]
fn session_events_list_scene_switches_pauses_and_resumes_in_order() {
    let mut s = headless();
    let first = s.collection().program_scene.0;
    assert_eq!(s.run("session.events", &json!({})).unwrap().0, json!({"events": [], "last": 0, "dropped": 0}));
    s.run("scene.add", &json!({"name": "Second"})).unwrap();
    let second = s.collection().program_scene.0;
    assert_ne!(first, second, "a new scene goes on the program");
    s.run("scene.select", &json!({"scene": "Second"})).unwrap();
    s.run("pause.start", &json!({"countdown_s": 0})).unwrap();
    s.run("scene.select", &json!({"scene": "Scene"})).unwrap();
    s.run("pause.extend", &json!({"seconds": 5})).unwrap();
    s.run("pause.stop", &json!({})).unwrap();
    s.run("studio.toggle", &json!({})).unwrap();
    s.run("scene.select", &json!({"scene": "Second"})).unwrap();
    s.run("studio.toggle", &json!({})).unwrap();
    let rev = s.engine().revision();
    let (v, _) = s.run("session.events", &json!({})).unwrap();
    assert_eq!(s.engine().revision(), rev, "reading the events changes nothing");
    let kinds: Vec<&str> = v["events"].as_array().map(|a| a.iter().filter_map(|e| e["kind"].as_str()).collect()).unwrap_or_default();
    // Selecting the scene the program already shows, adding time and choosing a preview scene in
    // studio mode are not switches.
    assert_eq!(kinds, ["scene", "pause", "scene", "resume"], "{v}");
    let e = &v["events"];
    assert_eq!(e[0]["from"], json!({"id": first, "name": "Scene"}));
    assert_eq!(e[0]["to"], json!({"id": second, "name": "Second"}));
    assert_eq!(e[2]["to"], json!({"id": first, "name": "Scene"}), "a switch under the pause screen is listed");
    assert_eq!(e[0]["recording_s"], Value::Null, "nothing was recorded");
    let seqs: Vec<u64> = e.as_array().map(|a| a.iter().filter_map(|e| e["seq"].as_u64()).collect()).unwrap_or_default();
    assert_eq!(seqs, [1, 2, 3, 4]);
    let times: Vec<u64> = e.as_array().map(|a| a.iter().filter_map(|e| e["time_ms"].as_u64()).collect()).unwrap_or_default();
    assert!(times.windows(2).all(|w| w[0] <= w[1]), "{times:?}");
    let (tail, _) = s.run("session.events", &json!({"since": 2})).unwrap();
    assert_eq!(tail["events"].as_array().map(Vec::len), Some(2));
    assert_eq!((tail["last"].clone(), tail["dropped"].clone()), (json!(4), json!(0)));
    // Removing the program scene switches to another one; the removed scene has no name left.
    s.run("scene.remove", &json!({"scene": "Scene"})).unwrap();
    let (v, _) = s.run("session.events", &json!({"since": 4})).unwrap();
    assert_eq!(v["events"][0]["from"], json!({"id": first, "name": null}), "{v}");
    assert_eq!(v["events"][0]["to"]["id"], json!(second));
    assert!(s.run("session.events", &json!({"since": -1})).is_err());
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
fn filter_meter_reads_a_compressor_and_is_null_for_a_filter_without_a_meter() {
    let mut s = headless();
    let (out, _) = s.run("source.add", &json!({"kind": "audio_input", "name": "Mic"})).unwrap();
    let mic = out["source"].as_u64().unwrap_or_else(|| panic!("{out}"));
    s.run("source.add", &json!({"kind": "color", "name": "Card"})).unwrap();
    let add = |s: &mut Studio, source: &str, kind: &str| {
        let (out, _) = s.run("filter.add", &json!({"source": source, "kind": kind})).unwrap();
        out["filter"].as_u64().unwrap_or_else(|| panic!("{out}"))
    };
    let (comp, gain, crop) = (add(&mut s, "Mic", "compressor"), add(&mut s, "Mic", "gain"), add(&mut s, "Card", "crop_pad"));
    let meter = |s: &mut Studio, source: serde_json::Value, filter: u64| s.run("filter.meter", &json!({"source": source, "filter": filter}));
    let (rev, dirty) = (s.engine().revision(), s.engine().is_dirty());
    for source in [json!("Mic"), json!(mic)] {
        assert_eq!(meter(&mut s, source, comp).unwrap(), (json!({"gain_reduction_db": 0.0}), None), "a compressor fed nothing");
    }
    assert_eq!(meter(&mut s, json!("Mic"), gain).unwrap().0, serde_json::Value::Null, "gain has no meter");
    assert_eq!(meter(&mut s, json!("Card"), crop).unwrap().0, serde_json::Value::Null, "a video filter");
    assert_eq!((s.engine().revision(), s.engine().is_dirty()), (rev, dirty), "a reading changes nothing");
    s.run("filter.enable", &json!({"source": "Mic", "filter": comp, "enabled": false})).unwrap();
    assert_eq!(meter(&mut s, json!("Mic"), comp).unwrap().0, serde_json::Value::Null, "a disabled filter does not run");
    s.run("filter.enable", &json!({"source": "Mic", "filter": comp, "enabled": true})).unwrap();
    assert_eq!(meter(&mut s, json!("Mic"), comp).unwrap().0, json!({"gain_reduction_db": 0.0}));
    assert!(meter(&mut s, json!("Card"), comp).is_err(), "a filter of another source");
    assert!(meter(&mut s, json!("Ghost"), comp).is_err());
    s.run("filter.remove", &json!({"source": "Mic", "filter": comp})).unwrap();
    assert!(meter(&mut s, json!("Mic"), comp).is_err(), "a removed filter");
}

#[test]
fn the_state_reports_how_long_each_source_s_audio_filters_delay_it() {
    let mut s = headless();
    let (out, _) = s.run("source.add", &json!({"kind": "audio_input", "name": "Mic"})).unwrap();
    let mic = out["source"].as_u64().unwrap_or_else(|| panic!("{out}"));
    let latency = |s: &Studio| s.state_json()["filter_latency"].clone();
    assert_eq!(latency(&s), json!({}), "no filter delays anything");
    let (out, _) = s.run("filter.add", &json!({"source": "Mic", "kind": "limiter"})).unwrap();
    let limiter = out["filter"].as_u64().unwrap_or_else(|| panic!("{out}"));
    s.run("filter.add", &json!({"source": "Mic", "kind": "gain"})).unwrap();
    assert_eq!(latency(&s), json!({mic.to_string(): {"frames": 248, "ms": 5.2}}));
    assert_eq!(s.filter_latency_frames(SourceId(mic)), 248);
    s.run("filter.enable", &json!({"source": "Mic", "filter": limiter, "enabled": false})).unwrap();
    assert_eq!(latency(&s), json!({}), "a disabled filter does not run");
    assert_eq!(s.filter_latency_frames(SourceId(mic)), 0);
    assert_eq!([latency_ms(0), latency_ms(480), latency_ms(1)], [0.0, 10.0, 0.0]);
}

#[test]
fn interface_commands_come_back_as_actions() {
    let mut s = headless();
    assert_eq!(s.run("app.settings", &json!({})).unwrap().1, Some(UiAction::OpenSettings));
    assert_eq!(s.run("app.exit", &json!({})).unwrap().1, Some(UiAction::Exit));
    assert_eq!(s.run("scene.add", &json!({})).unwrap().1, None);
    // Without a file the results are only forgotten for the session.
    assert_eq!(s.run("hardware_encoders.retest", &json!({})).unwrap(), (serde_json::Value::Null, None));
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
    let e = s.run("record.marker", &json!({"name": "Goal"})).unwrap_err();
    assert_eq!(e.to_string(), "neither a recording nor a stream's copy is running");
    let e = s.run("marker.add", &json!({"name": "Goal"})).unwrap_err();
    assert_eq!(e.to_string(), "no recording, stream copy or take of audio tracks is running");
    let (v, _) = s.run("marker.list", &json!({})).unwrap();
    assert_eq!(v, json!({"running": false, "markers": []}));
    let e = s.run("marker.rename", &json!({"index": 1, "name": "Goal"})).unwrap_err();
    assert_eq!(e.to_string(), "there is no marker 1: the session holds no markers");
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

/// The go-live check's item `id` in the result `r`.
fn golive_item<'a>(r: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    r["items"].as_array().and_then(|a| a.iter().find(|i| i["id"] == id)).unwrap_or_else(|| panic!("no item {id} in {r}"))
}

#[test]
fn the_golive_check_lists_each_item_and_changes_nothing() {
    use studiocraft_model::{AudioState, Destination, KeyRef, SourceConfig};
    let mut c = SceneCollection::new("Test", "Scene");
    let mut ids = Vec::new();
    for (kind, name) in [("no_such_plugin", "Old plug-in"), ("captions", "Captions")] {
        let id = SourceId(c.alloc_id());
        ids.push(id);
        c.sources.push(SourceConfig {
            id,
            kind: kind.into(),
            kind_version: 1,
            name: name.into(),
            settings: Settings::new(),
            filters: vec![],
            audio: AudioState::default(),
        });
    }
    let mut p = Profile::default();
    p.output.folder = std::env::temp_dir().display().to_string();
    let mut s = studio_with(Some(noop_gpu()), c, p);
    // A wait with no time limit, so the check takes the reader's answer.
    s.golive.set_space_reader(Arc::new(|_p: &Path| Ok(golive::MIN_FREE_BYTES * 2)), Duration::MAX);
    let (rev, dirty) = (s.engine().revision(), s.engine().is_dirty());

    let r = s.run("golive.check", &json!({})).unwrap().0;
    assert_eq!(r["ready"], json!(false));
    let d = golive_item(&r, "destinations");
    assert_eq!((&d["state"], &d["fix"], &d["text"]), (&json!("problem"), &json!("view.streaming"), &json!("No stream destination is turned on")));
    assert_eq!(golive_item(&r, "encoder")["state"], json!("pass"), "{r}");
    let space = golive_item(&r, "recording_space");
    assert_eq!((&space["state"], &space["free_bytes"]), (&json!("pass"), &json!(golive::MIN_FREE_BYTES * 2)));
    let old = golive_item(&r, &format!("source:{}", ids[0].0));
    assert_eq!((&old["state"], &old["fix"], &old["source"]), (&json!("problem"), &json!(null), &json!(ids[0].0)));
    assert_eq!(old["args"]["kind"], json!("no_such_plugin"));
    let captions = golive_item(&r, "captions");
    assert_eq!((&captions["state"], &captions["fix"]), (&json!("problem"), &json!("captions.toggle")), "a Live Captions source and captions off");
    let problems = r["items"].as_array().map_or(0, |a| a.iter().filter(|i| i["state"] == "problem").count());
    assert!(problems >= 3, "{r}");
    assert_eq!((r["problems"].as_u64(), r["more_sources"].as_u64()), (Some(problems as u64), Some(0)), "{r}");

    // A destination whose session key is not held is a problem until the key is held, and then
    // unknown: it has not connected yet.
    let mut p = s.profile().clone();
    p.stream.destinations = vec![Destination {
        id: "main".into(),
        server: "rtmp://example.invalid/live".into(),
        key: KeyRef::Held("k".into()),
        ..Destination::default()
    }];
    s.set_profile(p).unwrap();
    let r = s.run("golive.check", &json!({})).unwrap().0;
    assert_eq!(golive_item(&r, "destination:main")["state"], json!("problem"));
    s.hold_stream_key("k", "secret");
    let r = s.run("golive.check", &json!({})).unwrap().0;
    let d = golive_item(&r, "destination:main");
    assert_eq!((&d["state"], &d["fix"], &d["destination"]), (&json!("unknown"), &json!(null), &json!("main")));
    assert!(!r.to_string().contains("secret"), "the key is never in the result");

    assert_eq!((s.engine().revision(), s.engine().is_dirty()), (rev, dirty), "the check changes no collection");
    assert!(s.engine().undo_label().is_none(), "and adds no undo step");
    assert!(s.run("golive.check", &json!({"fix": true})).is_err());
}

/// A thread that hands over each connection `listener` takes.
fn connections(listener: std::net::TcpListener) -> std::sync::mpsc::Receiver<std::net::TcpStream> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for s in listener.incoming().flatten() {
            if tx.send(s).is_err() {
                return;
            }
        }
    });
    rx
}

#[test]
fn a_running_streams_destinations_start_and_stop_one_at_a_time() {
    use studiocraft_model::{Destination, KeyRef};
    use studiocraft_plugin::DestinationStatus;
    // Only a broken build waits this long for a connection that should come at once.
    const GUARD: Duration = Duration::from_secs(60);
    let listen = || {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("rtmp://127.0.0.1:{}/live", l.local_addr().unwrap().port());
        (url, connections(l))
    };
    let ((a_url, a), (b_url, b)) = (listen(), listen());
    let mut p = small_profile();
    p.stream.key = "profile-key".into();
    p.stream.destinations = vec![
        Destination { id: "a".into(), server: a_url, key: KeyRef::Profile, ..Destination::default() },
        Destination { id: "b".into(), server: b_url, key: KeyRef::Held("b".into()), enabled: false, ..Destination::default() },
    ];
    let mut s = studio_with(Some(noop_gpu()), SceneCollection::new("Test", "Scene"), p);
    let Err(e) = s.set_stream_destination("a", true) else { panic!("a destination started without a stream") };
    assert!(e.to_string().contains("the stream is not running"), "{e}");
    assert!(s.stream_destinations().is_empty());
    assert_eq!(s.stream_profile(), None);
    s.start_output(OutputKind::Streaming).unwrap();
    let a_conn = a.recv_timeout(GUARD).expect("the destination that is on connects at the start");
    let name = s.profile().name.clone();
    assert_eq!(s.stream_profile(), Some(name.as_str()), "the stream names the profile it started with");
    let status = |s: &Studio, id: &str| s.stream_destinations().into_iter().find(|d| d.id == id).map(|d| d.status);
    let ids: Vec<String> = s.stream_destinations().into_iter().map(|d| d.id).collect();
    assert_eq!(ids, ["a", "b"], "in the profile's order");
    assert_eq!(status(&s, "b"), Some(DestinationStatus::Stopped), "a destination that is off is not started");
    // Its held key is not set, so it does not start.
    let Err(e) = s.set_stream_destination("b", true) else { panic!("started without its held key") };
    assert!(e.to_string().contains("the stream key `b` for destination `b` is not set for this session"), "{e}");
    assert_eq!(status(&s, "b"), Some(DestinationStatus::Stopped));
    // A key held after the stream started is used.
    assert!(!s.stream_key_held("b"));
    s.hold_stream_key("b", "late");
    assert!(s.stream_key_held("b") && !s.stream_key_held("late"));
    s.set_stream_destination("b", true).unwrap();
    let b_conn = b.recv_timeout(GUARD).expect("the destination started connects");
    assert!(!matches!(status(&s, "b"), Some(DestinationStatus::Stopped | DestinationStatus::Failed(_))), "{:?}", status(&s, "b"));
    // Stopped while it connects; the outputs crate's tests follow the destination until it ends.
    s.set_stream_destination("b", false).unwrap();
    drop(b_conn);
    // The other destination goes on, and the output keeps running.
    assert!(!matches!(status(&s, "a"), Some(DestinationStatus::Stopped | DestinationStatus::Failed(_))), "{:?}", status(&s, "a"));
    assert!(s.output(OutputKind::Streaming).is_some_and(|o| o.active));
    // A forgotten key is looked up again, and an unknown destination is an error.
    s.hold_stream_key("b", "");
    assert!(!s.stream_key_held("b"));
    assert!(s.set_stream_destination("b", true).is_err(), "the held key was forgotten");
    assert!(s.set_stream_destination("x", true).is_err() && s.set_stream_destination("x", false).is_err());
    // Another profile put in use keeps the running stream's profile name.
    let mut other = s.profile().clone();
    other.name = "Other".into();
    s.set_profile(other).unwrap();
    assert_eq!(s.stream_profile(), Some(name.as_str()));
    s.stream_profile_renamed("Other", "Third");
    assert_eq!(s.stream_profile(), Some(name.as_str()), "only the stream's own profile is followed");
    s.stream_profile_renamed(&name, "Renamed");
    assert_eq!(s.stream_profile(), Some("Renamed"));
    // Once its profile is removed, the stream belongs to none, even to a new profile of that name.
    s.stream_profile_removed("Other");
    assert_eq!(s.stream_profile(), Some("Renamed"));
    s.stream_profile_removed("Renamed");
    assert_eq!(s.stream_profile(), None);
    s.stream_profile_renamed("Renamed", "Again");
    assert_eq!(s.stream_profile(), None);
    drop(a_conn);
    s.stop_output(OutputKind::Streaming);
    assert!(s.stream_destinations().is_empty(), "no stream runs");
    assert_eq!(s.stream_profile(), None);
}

#[test]
fn a_streams_automatic_check_follows_its_profile_and_ends_with_it() {
    use studiocraft_model::{Destination, KeyRef};
    // Only a broken build waits this long for a connection that should come at once.
    const GUARD: Duration = Duration::from_secs(60);
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("rtmp://127.0.0.1:{}/live", l.local_addr().unwrap().port());
    let incoming = connections(l);
    let mut p = small_profile();
    p.name = "Checked Show".into();
    p.stream.key = "profile-key".into();
    p.stream.reconnect.enabled = false;
    p.stream.destinations = vec![Destination { id: "a".into(), server: url, key: KeyRef::Profile, ..Destination::default() }];
    let mut s = studio_with(Some(noop_gpu()), SceneCollection::new("Test", "Scene"), p);
    s.start_output(OutputKind::Streaming).unwrap();
    // The stream's connection is closed, so the destination gives up and its connection is
    // checked: the check connects to the same port.
    drop(incoming.recv_timeout(GUARD).expect("the stream connects"));
    let _probe = incoming.recv_timeout(GUARD).expect("the check connects");
    let automatic = |s: &Studio| match s.stream_destination_check("a") {
        Some(diagnose::CheckState::Running { automatic }) => Some(automatic),
        Some(diagnose::CheckState::Done(d)) => Some(d.automatic),
        None => None,
    };
    assert_eq!(automatic(&s), Some(true));
    // It stays shown after the stream ended, and with the stream's profile after a rename.
    s.stop_output(OutputKind::Streaming);
    assert!(s.stream_destinations().is_empty(), "no stream runs");
    assert_eq!(automatic(&s), Some(true));
    // Starts that fail do not push the stream's check out of those kept.
    let first = s.profile().clone();
    let mut failing = first.clone();
    failing.stream.video_encoder = "x264".into();
    s.set_profile(failing).unwrap();
    let mut spins = 0u64;
    let mut failed = 0;
    while failed <= 2 * super::outputs::MAX_CHECK_KEYS {
        let Err(e) = s.start_output(OutputKind::Streaming) else { panic!("a stream started with an encoder that runs in ffmpeg") };
        if e.to_string().contains("is still being finished") {
            spins += 1;
            assert!(spins < 200_000_000, "the first stream never finished");
            s.tick();
            std::thread::yield_now();
        } else {
            failed += 1;
        }
    }
    s.set_profile(first.clone()).unwrap();
    assert_eq!(automatic(&s), Some(true), "the check stays shown after starts that failed");
    // A stream of another profile keeps its own checks, and this profile's stay.
    let mut second = first.clone();
    second.name = "Second Show".into();
    s.set_profile(second).unwrap();
    assert_eq!(automatic(&s), None, "another profile's destination `a` is another destination");
    // The first stream finishes before the next one starts.
    let mut spins = 0u64;
    while let Err(e) = s.start_output(OutputKind::Streaming) {
        assert!(e.to_string().contains("is still being finished"), "{e}");
        spins += 1;
        assert!(spins < 200_000_000, "the first stream never finished");
        s.tick();
        std::thread::yield_now();
    }
    drop(incoming.recv_timeout(GUARD).expect("the second stream connects"));
    let _second_probe = incoming.recv_timeout(GUARD).expect("its check connects");
    assert_eq!(automatic(&s), Some(true));
    s.stop_output(OutputKind::Streaming);
    s.set_profile(first).unwrap();
    assert_eq!(automatic(&s), Some(true));
    let mut renamed = s.profile().clone();
    renamed.name = "Renamed Show".into();
    s.set_profile(renamed.clone()).unwrap();
    s.stream_profile_renamed("Checked Show", "Renamed Show");
    assert_eq!(automatic(&s), Some(true));
    // Once its profile is removed, a new profile of the same name does not show it.
    s.stream_profile_removed("Renamed Show");
    s.set_profile(renamed).unwrap();
    assert_eq!(automatic(&s), None);
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
fn a_new_canvas_lays_out_the_scenes_as_one_undo_step() {
    let mut s = headless();
    // An image without a file never shows a picture, so it is placed by its corner.
    s.run("source.add", &json!({"kind": "image", "name": "Logo", "transform": {"position": {"x": 960.0, "y": 540.0}}})).unwrap();
    s.seal_history();
    let at = |s: &Studio| s.collection().scenes[0].items[0].transform;
    let mut p = s.profile().clone();
    p.video.base_width = 1280;
    p.video.base_height = 720;
    s.set_profile(p.clone()).unwrap();
    let t = at(&s);
    assert!((t.position.x - 640.0).abs() < 0.01 && (t.position.y - 360.0).abs() < 0.01, "{t:?}");
    assert!((t.scale.x - 2.0 / 3.0).abs() < 0.001, "{t:?}");
    // Other settings leave the layout alone.
    p.video.fps_num = 60;
    s.set_profile(p).unwrap();
    assert_eq!(at(&s), t);
    s.run("edit.undo", &json!({})).unwrap();
    assert_eq!(at(&s).position.x, 960.0, "one undo step takes the layout back");
}

#[test]
fn a_filled_source_that_shows_nothing_at_a_canvas_change_fills_it_once_it_shows_a_picture() {
    let mut s = headless();
    let r = s.run("source.add", &json!({"kind": "color", "name": "Back", "settings": {"width": 400, "height": 300}})).unwrap();
    let item = r.0["item"].clone();
    let source = s.collection().scenes[0].items[0].source;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    assert!(s.notify_source_shown(source, tx));
    rx.recv().unwrap();
    let native = s.source_size(source).unwrap();
    let fill = studiocraft_model::Transform::fill(native, (1920, 1080));
    s.run("item.transform", &json!({"item": item, "transform": fill, "fit": "fill"})).unwrap();
    // The source loses its picture just before the canvas changes.
    let video = Arc::clone(&s.sources.io(source).unwrap().video);
    video.publish(studiocraft_plugin::VideoContent::Empty);
    assert_eq!(s.source_size(source), None);
    let mut p = s.profile().clone();
    (p.video.base_width, p.video.base_height) = (1080, 1920);
    s.set_profile(p).unwrap();
    let at = |s: &Studio| s.collection().scenes[0].items[0].transform;
    assert_ne!(at(&s), studiocraft_model::Transform::fill(native, (1080, 1920)));
    s.tick();
    assert_ne!(at(&s), studiocraft_model::Transform::fill(native, (1080, 1920)), "nothing to place by yet");
    // A picture too large to lay out by is not placed by; one of another size that follows is.
    video.publish(studiocraft_plugin::VideoContent::Solid { rgba: [0; 4], width: 70_000, height: 300 });
    s.tick();
    assert_ne!(at(&s), studiocraft_model::Transform::fill(native, (1080, 1920)), "too large to place by");
    // When the picture comes back, the item fills the new canvas.
    video.publish(studiocraft_plugin::VideoContent::Solid { rgba: [0; 4], width: 400, height: 300 });
    s.tick();
    assert_eq!(at(&s), studiocraft_model::Transform::fill(native, (1080, 1920)));
    assert_eq!(s.collection().scenes[0].items[0].fit, Some(studiocraft_model::FitMode::Fill));
    // Once placed, it is not placed again: a move made later stays.
    let moved = studiocraft_model::Transform { position: studiocraft_model::Vec2::new(5.0, 5.0), ..at(&s) };
    s.run("item.transform", &json!({"item": item, "transform": moved})).unwrap();
    s.tick();
    assert_eq!(at(&s), moved);

    // A layout that is undone, or taken back after a failed save, has nothing left to place.
    let fill = studiocraft_model::Transform::fill(native, (1080, 1920));
    s.run("item.transform", &json!({"item": item, "transform": fill, "fit": "fill"})).unwrap();
    s.seal_history();
    let tall = s.profile().clone();
    let mut wide = tall.clone();
    (wide.video.base_width, wide.video.base_height) = (1920, 1080);
    video.publish(studiocraft_plugin::VideoContent::Empty);
    s.set_profile(wide.clone()).unwrap();
    s.run("edit.undo", &json!({})).unwrap();
    video.publish(studiocraft_plugin::VideoContent::Solid { rgba: [0; 4], width: 400, height: 300 });
    s.tick();
    assert_eq!(at(&s), fill, "undo took the layout back");
    // Redoing the layout puts the item back in the wait.
    video.publish(studiocraft_plugin::VideoContent::Empty);
    s.run("edit.redo", &json!({})).unwrap();
    video.publish(studiocraft_plugin::VideoContent::Solid { rgba: [0; 4], width: 400, height: 300 });
    s.tick();
    assert_eq!(at(&s), studiocraft_model::Transform::fill(native, (1920, 1080)), "redo waits again");
    s.run("edit.undo", &json!({})).unwrap();
    s.run("edit.undo", &json!({})).unwrap();
    assert_eq!(at(&s), fill);
    // Undoing an unrelated edit leaves the wait as it is.
    s.set_profile(tall.clone()).unwrap();
    assert_eq!(at(&s), fill);
    video.publish(studiocraft_plugin::VideoContent::Empty);
    s.set_profile(wide.clone()).unwrap();
    s.run("scene.add", &json!({"name": "Other"})).unwrap();
    s.run("edit.undo", &json!({})).unwrap();
    video.publish(studiocraft_plugin::VideoContent::Solid { rgba: [0; 4], width: 400, height: 300 });
    s.tick();
    assert_eq!(at(&s), studiocraft_model::Transform::fill(native, (1920, 1080)), "an unrelated undo keeps the wait");
    s.run("edit.undo", &json!({})).unwrap();
    s.run("edit.undo", &json!({})).unwrap();
    assert_eq!(at(&s), fill);
    s.set_profile(tall.clone()).unwrap();
    assert_eq!(at(&s), fill);
    video.publish(studiocraft_plugin::VideoContent::Empty);
    s.set_profile(wide).unwrap();
    s.restore_profile(tall).unwrap();
    video.publish(studiocraft_plugin::VideoContent::Solid { rgba: [0; 4], width: 400, height: 300 });
    s.tick();
    assert_eq!(at(&s), fill, "the restore took the layout back");
}

#[test]
fn a_filled_source_flipped_while_it_waits_for_a_picture_still_fills_the_new_canvas() {
    let mut s = headless();
    let r = s.run("source.add", &json!({"kind": "color", "name": "Back", "settings": {"width": 400, "height": 300}})).unwrap();
    let item = r.0["item"].clone();
    let source = s.collection().scenes[0].items[0].source;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    assert!(s.notify_source_shown(source, tx));
    rx.recv().unwrap();
    let native = s.source_size(source).unwrap();
    s.run("item.transform", &json!({"item": item, "transform": studiocraft_model::Transform::fill(native, (1920, 1080)), "fit": "fill"})).unwrap();
    let video = Arc::clone(&s.sources.io(source).unwrap().video);
    video.publish(studiocraft_plugin::VideoContent::Empty);
    let mut p = s.profile().clone();
    (p.video.base_width, p.video.base_height) = (1080, 1920);
    s.set_profile(p).unwrap();
    s.seal_history();
    // The flip keeps the fit mode, so the item still waits to fill the new canvas.
    let at = |s: &Studio| s.collection().scenes[0].items[0].transform;
    let flipped = studiocraft_model::Transform { flip_horizontal: true, ..at(&s) };
    s.run("item.transform", &json!({"item": item, "transform": flipped, "fit": "fill"})).unwrap();
    video.publish(studiocraft_plugin::VideoContent::Solid { rgba: [0; 4], width: 400, height: 300 });
    s.tick();
    let want = studiocraft_model::Transform { flip_horizontal: true, ..studiocraft_model::Transform::fill(native, (1080, 1920)) };
    assert_eq!(at(&s), want);
    // A move made while it waits still leaves it where it is.
    video.publish(studiocraft_plugin::VideoContent::Empty);
    let mut p = s.profile().clone();
    (p.video.base_width, p.video.base_height) = (1920, 1080);
    s.set_profile(p).unwrap();
    s.seal_history();
    let moved = studiocraft_model::Transform { position: studiocraft_model::Vec2::new(5.0, 5.0), flip_vertical: true, ..at(&s) };
    s.run("item.transform", &json!({"item": item, "transform": moved, "fit": "fill"})).unwrap();
    video.publish(studiocraft_plugin::VideoContent::Solid { rgba: [0; 4], width: 400, height: 300 });
    s.tick();
    assert_eq!(at(&s), moved);
}

#[test]
fn restoring_a_profile_also_takes_back_placements_of_items_that_waited() {
    let mut s = headless();
    let r = s.run("source.add", &json!({"kind": "color", "name": "Back", "settings": {"width": 400, "height": 300}})).unwrap();
    let item = r.0["item"].clone();
    let source = s.collection().scenes[0].items[0].source;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    assert!(s.notify_source_shown(source, tx));
    rx.recv().unwrap();
    let fill = studiocraft_model::Transform::fill((400, 300), (1920, 1080));
    s.run("item.transform", &json!({"item": item, "transform": fill, "fit": "fill"})).unwrap();
    s.seal_history();
    s.mark_saved();
    let video = Arc::clone(&s.sources.io(source).unwrap().video);
    video.publish(studiocraft_plugin::VideoContent::Empty);
    let previous = s.profile().clone();
    let mut p = previous.clone();
    (p.video.base_width, p.video.base_height) = (1080, 1920);
    s.set_profile(p).unwrap();
    video.publish(studiocraft_plugin::VideoContent::Solid { rgba: [0; 4], width: 400, height: 300 });
    s.tick();
    let at = |s: &Studio| s.collection().scenes[0].items[0].transform;
    assert_eq!(at(&s), studiocraft_model::Transform::fill((400, 300), (1080, 1920)));
    s.restore_profile(previous).unwrap();
    assert_eq!(at(&s), fill);
    assert!(!s.engine().is_dirty(), "the collection is clean again");
    assert_eq!(s.engine().undo_label(), Some("Transform Source"), "no layout step is left");
}

#[test]
fn restoring_a_profile_keeps_a_change_made_after_its_layout() {
    let mut s = headless();
    s.run("source.add", &json!({"kind": "image", "name": "Logo", "transform": {"position": {"x": 960.0, "y": 540.0}}})).unwrap();
    s.run("scene.add", &json!({"name": "Other"})).unwrap();
    s.seal_history();
    s.mark_saved();
    let previous = s.profile().clone();
    let mut p = previous.clone();
    (p.video.base_width, p.video.base_height) = (1080, 1920);
    s.set_profile(p).unwrap();
    let other = s.collection().scenes.iter().map(|sc| sc.id).find(|id| *id != s.collection().program_scene).unwrap();
    s.run("scene.select", &json!({"scene": other.0})).unwrap();
    let shown = s.collection().program_scene;
    assert_eq!(shown, other);
    s.restore_profile(previous).unwrap();
    assert_eq!(s.collection().program_scene, shown, "the scene shown since stays");
    assert!(s.engine().is_dirty(), "the change is still unsaved");

    // A layout undone and redone before the restore is still taken back.
    s.mark_saved();
    let previous = s.profile().clone();
    let before = s.collection().clone();
    let newest = s.engine().last_step();
    let mut p = previous.clone();
    (p.video.base_width, p.video.base_height) = (1280, 720);
    s.set_profile(p).unwrap();
    s.run("edit.undo", &json!({})).unwrap();
    s.run("edit.redo", &json!({})).unwrap();
    s.restore_profile(previous).unwrap();
    assert_eq!(s.collection().scenes, before.scenes);
    assert!(!s.engine().is_dirty());
    assert_eq!(s.engine().last_step(), newest, "no step is left for the layout");
}

#[test]
fn an_undone_layout_places_no_waiting_item_that_it_left_in_place() {
    let mut s = headless();
    let mut added = Vec::new();
    for name in ["Moved", "Waiting"] {
        let r = s.run("source.add", &json!({"kind": "color", "name": name, "settings": {"width": 400, "height": 300}})).unwrap();
        let item = studiocraft_model::ItemId(r.0["item"].as_u64().unwrap());
        let source = s.collection().scenes[0].item(item).unwrap().source;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        assert!(s.notify_source_shown(source, tx));
        rx.recv().unwrap();
        added.push((item, Arc::clone(&s.sources.io(source).unwrap().video)));
    }
    let ((moved, _), (waiting, video)) = (added[0].clone(), added[1].clone());
    let centered = studiocraft_model::Transform { position: studiocraft_model::Vec2::new(960.0, 540.0), ..Default::default() };
    s.run("item.transform", &json!({"item": moved.0, "transform": centered})).unwrap();
    s.run("item.transform", &json!({"item": waiting.0, "transform": studiocraft_model::Transform::default(), "fit": "fill"})).unwrap();
    s.seal_history();
    video.publish(studiocraft_plugin::VideoContent::Empty);
    let mut p = s.profile().clone();
    (p.video.base_width, p.video.base_height) = (1080, 1920);
    s.set_profile(p).unwrap();
    let at = |s: &Studio, item| s.collection().scenes[0].item(item).unwrap().transform;
    assert_ne!(at(&s, moved), centered);
    assert_eq!(at(&s, waiting), studiocraft_model::Transform::default(), "the layout left it in place");
    s.run("edit.undo", &json!({})).unwrap();
    assert_eq!(at(&s, moved), centered);
    // A move made after the undo does not bring the layout back.
    let elsewhere = studiocraft_model::Transform { position: studiocraft_model::Vec2::new(10.0, 10.0), ..Default::default() };
    s.run("item.transform", &json!({"item": moved.0, "transform": elsewhere})).unwrap();
    video.publish(studiocraft_plugin::VideoContent::Solid { rgba: [0; 4], width: 400, height: 300 });
    s.tick();
    assert_eq!(at(&s, waiting), studiocraft_model::Transform::default(), "the undo took the layout back");
}

#[test]
fn items_waiting_for_a_picture_keep_waiting_through_other_undo_steps() {
    let mut s = headless();
    let mut added = Vec::new();
    for name in ["A", "B"] {
        let r = s.run("source.add", &json!({"kind": "color", "name": name, "settings": {"width": 400, "height": 300}})).unwrap();
        let item = studiocraft_model::ItemId(r.0["item"].as_u64().unwrap());
        let source = s.collection().scenes[0].item(item).unwrap().source;
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        assert!(s.notify_source_shown(source, tx));
        rx.recv().unwrap();
        let fill = studiocraft_model::Transform::fill((400, 300), (1920, 1080));
        s.run("item.transform", &json!({"item": item, "transform": fill, "fit": "fill"})).unwrap();
        s.seal_history();
        added.push((item, Arc::clone(&s.sources.io(source).unwrap().video)));
    }
    let at = |s: &Studio, item| s.collection().scenes[0].item(item).unwrap().transform;
    let tall = studiocraft_model::Transform::fill((400, 300), (1080, 1920));
    let picture = || studiocraft_plugin::VideoContent::Solid { rgba: [0; 4], width: 400, height: 300 };
    let ((a, a_video), (b, b_video)) = (added[0].clone(), added[1].clone());
    a_video.publish(studiocraft_plugin::VideoContent::Empty);
    b_video.publish(studiocraft_plugin::VideoContent::Empty);
    let mut p = s.profile().clone();
    (p.video.base_width, p.video.base_height) = (1080, 1920);
    s.set_profile(p).unwrap();
    let relaid = at(&s, a);
    // A is placed first; undoing that placement leaves B waiting.
    a_video.publish(picture());
    s.tick();
    assert_eq!(at(&s, a), tall);
    s.run("edit.undo", &json!({})).unwrap();
    assert_eq!(at(&s, a), relaid);
    b_video.publish(picture());
    s.tick();
    assert_eq!(at(&s, b), tall, "B still waited");
    assert_eq!(at(&s, a), relaid, "an undone placement is not made again");
    // A waiting item that is removed and brought back by undo still waits.
    b_video.publish(studiocraft_plugin::VideoContent::Empty);
    let mut p = s.profile().clone();
    (p.video.base_width, p.video.base_height) = (1920, 1080);
    s.set_profile(p).unwrap();
    let wide = studiocraft_model::Transform::fill((400, 300), (1920, 1080));
    assert_ne!(at(&s, b), wide);
    s.run("item.remove", &json!({"item": b.0})).unwrap();
    s.tick();
    s.run("edit.undo", &json!({})).unwrap();
    let source = s.collection().scenes[0].item(b).unwrap().source;
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    assert!(s.notify_source_shown(source, tx));
    rx.recv().unwrap();
    s.sources.io(source).unwrap().video.publish(picture());
    s.tick();
    assert_eq!(at(&s, b), wide, "the item that came back was placed");
    // Another mode set while the item waits ends the wait.
    b_video.publish(studiocraft_plugin::VideoContent::Empty);
    s.sources.io(source).unwrap().video.publish(studiocraft_plugin::VideoContent::Empty);
    let mut p = s.profile().clone();
    (p.video.base_width, p.video.base_height) = (1080, 1920);
    s.set_profile(p).unwrap();
    let waiting = at(&s, b);
    s.run("item.transform", &json!({"item": b.0, "transform": waiting, "fit": "fit"})).unwrap();
    s.sources.io(source).unwrap().video.publish(picture());
    s.tick();
    assert_eq!(at(&s, b), waiting, "the edit ended the wait");
}

#[test]
fn restoring_a_profile_takes_back_its_layout_change_without_a_history_step() {
    let mut s = headless();
    s.run("source.add", &json!({"kind": "image", "name": "Logo", "transform": {"position": {"x": 960.0, "y": 540.0}}})).unwrap();
    s.seal_history();
    let at = |s: &Studio| s.collection().scenes[0].items[0].transform;
    let before = at(&s);
    s.mark_saved();
    let previous = s.profile().clone();
    let mut p = previous.clone();
    (p.video.base_width, p.video.base_height) = (1280, 720);
    s.set_profile(p.clone()).unwrap();
    assert_ne!(at(&s), before);
    assert!(s.engine().is_dirty());
    // The new profile could not be saved: the old one comes back with its layout, and the
    // history holds no step for either change.
    s.restore_profile(previous.clone()).unwrap();
    assert_eq!(s.canvas(), (1920, 1080));
    assert_eq!(at(&s), before);
    assert!(!s.engine().is_dirty(), "a saved collection stays saved");
    assert_eq!(s.engine().redo_label(), None, "the layout change is not left to redo");
    s.run("edit.undo", &json!({})).unwrap();
    assert!(s.collection().scenes[0].items.is_empty(), "the next undo takes back the edit before the canvas change");

    // An edit after the canvas change keeps its own step, and the restore lays the scenes out
    // again instead.
    s.run("edit.redo", &json!({})).unwrap();
    s.seal_history();
    s.set_profile(p).unwrap();
    let item = s.collection().scenes[0].items[0].id;
    s.run("item.visible", &json!({"item": item, "visible": false})).unwrap();
    s.restore_profile(previous).unwrap();
    assert_eq!(s.canvas(), (1920, 1080));
    assert!(!s.collection().scenes[0].items[0].visible, "the later edit stays");
    let t = at(&s);
    assert!((t.position.x - 960.0).abs() < 0.01 && (t.scale.x - 1.0).abs() < 0.001, "{t:?}");
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
    let (marker, _) = s.run("record.marker", &json!({"name": "Goal"})).unwrap();
    assert_eq!(marker["name"], json!("Goal"));
    std::thread::sleep(Duration::from_millis(1500));
    s.tick();
    s.run("record.stop", &json!({})).unwrap();
    let until = Instant::now() + Duration::from_secs(30);
    let (mut saved, mut markers) = (None, None);
    while Instant::now() < until && (saved.is_none() || markers.is_none()) {
        s.tick();
        for n in s.take_notices() {
            if n.text().starts_with("Recording saved") {
                saved = Some(n);
            } else if n.text().starts_with("Markers saved") {
                markers = Some(n);
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let saved = saved.expect("the recording finishes");
    assert!(!saved.error);
    let size = std::fs::metadata(&state.target).map(|m| m.len()).unwrap_or(0);
    assert!(size > 1000, "{} is {size} bytes", state.target);
    assert!(markers.is_some_and(|n| !n.error), "the marker file is written");
    let timeline = std::fs::read_to_string(std::path::Path::new(&state.target).with_extension("otio")).unwrap();
    assert!(timeline.contains("\"Goal\""), "{timeline}");
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
    fn safe_mode_keeps_the_monitoring_device_closed_until_it_is_left() {
        let ffmpeg: FfmpegSetting = Arc::new(RwLock::new(String::new()));
        let captions = Arc::new(CaptionBoard::new());
        let registry = Arc::new(builtin_registry(&ffmpeg, &captions).unwrap());
        let mut s = Studio::new_in_safe_mode(None, default_collection("Test"), Profile::default(), registry, ffmpeg, captions);
        let sink = Arc::new(FakeSink::default());
        s.set_audio_sink(Arc::clone(&sink) as Arc<dyn AudioSink>);
        monitor(&mut s, "Microphone", "monitor_only");
        assert!(sink.opened().is_empty() && s.monitoring().is_none(), "a monitored source opens no device in Safe Mode");
        s.leave_safe_mode();
        assert_eq!(sink.opened(), ["default"]);
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
fn loudness_readings_are_numbers_or_null_with_the_target() {
    use studiocraft_engine::mixer::LoudnessReading;
    use studiocraft_model::LoudnessTarget;
    let target = LoudnessTarget::default();
    let silent = LoudnessReading {
        momentary_lufs: f64::NEG_INFINITY,
        short_term_lufs: f64::NEG_INFINITY,
        integrated_lufs: f64::NEG_INFINITY,
        range_lu: 0.0,
        true_peak_dbtp: f64::NEG_INFINITY,
        generation: 0,
    };
    assert_eq!(
        loudness_json(&silent, &target),
        json!({
            "momentary_lufs": null, "short_term_lufs": null, "integrated_lufs": null, "range_lu": 0.0, "true_peak_dbtp": null,
            "generation": 0, "target": {"low_lufs": -18.0, "high_lufs": -14.0},
        })
    );
    let playing = LoudnessReading {
        momentary_lufs: -15.5,
        short_term_lufs: -16.25,
        integrated_lufs: -17.0,
        range_lu: 4.5,
        true_peak_dbtp: -1.5,
        generation: 940,
    };
    let v = loudness_json(&playing, &LoudnessTarget { low_lufs: -24.0, high_lufs: -22.0 });
    assert_eq!(
        v,
        json!({
            "momentary_lufs": -15.5, "short_term_lufs": -16.25, "integrated_lufs": -17.0, "range_lu": 4.5, "true_peak_dbtp": -1.5,
            "generation": 940, "target": {"low_lufs": -24.0, "high_lufs": -22.0},
        })
    );
    let nan = LoudnessReading { momentary_lufs: f64::NAN, true_peak_dbtp: f64::INFINITY, ..playing };
    let v = loudness_json(&nan, &target);
    assert_eq!((v["momentary_lufs"].clone(), v["true_peak_dbtp"].clone()), (json!(null), json!(null)), "only numbers JSON can hold");
}

#[test]
fn audio_loudness_reports_the_mix_and_the_profile_target() {
    let mut s = headless();
    let (v, ui) = s.run("audio.loudness", &json!({})).unwrap();
    assert!(ui.is_none());
    assert_eq!(v["target"], json!({"low_lufs": -18.0, "high_lufs": -14.0}), "the default band: {v}");
    for key in ["momentary_lufs", "short_term_lufs", "integrated_lufs", "range_lu", "true_peak_dbtp", "generation"] {
        assert!(v.get(key).is_some(), "{key} in {v}");
    }
    let mut p = s.profile().clone();
    p.audio.loudness_target = studiocraft_model::LoudnessTarget { low_lufs: -24.0, high_lufs: -22.0 };
    s.set_profile(p.clone()).unwrap();
    let (v, _) = s.run("audio.loudness", &json!({})).unwrap();
    assert_eq!(v["target"], json!({"low_lufs": -24.0, "high_lufs": -22.0}), "the profile in use");
    for (low_lufs, high_lufs) in [(-14.0, -18.0), (-70.0, -14.0), (f64::NAN, -14.0)] {
        let mut wrong = p.clone();
        wrong.audio.loudness_target = studiocraft_model::LoudnessTarget { low_lufs, high_lufs };
        let e = s.set_profile(wrong).unwrap_err().to_string();
        assert!(e.contains("loudness target"), "{e}");
    }
    assert_eq!(s.profile().audio.loudness_target, p.audio.loudness_target, "a refused band leaves the profile as it was");
    assert!(s.run("audio.loudness", &json!({"reset": true})).is_err(), "an unknown parameter is an error");
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
    fn a_streams_take_is_named_after_its_copy() {
        use studiocraft_model::{Destination, KeyRef};
        // Only a broken build waits this long for a connection that should come at once.
        const GUARD: Duration = Duration::from_secs(60);
        let dir = std::env::temp_dir().join(format!("studiocraft-tracks-copy-name-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // The names of the next two minutes are taken, so the copy's name has a number.
        let now = chrono::Local::now();
        for s in 0..120 {
            let stamp = (now + chrono::TimeDelta::seconds(s)).format("%Y-%m-%d %H-%M-%S");
            std::fs::write(dir.join(format!("Stream {stamp}.mkv")), b"older").unwrap();
        }
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("rtmp://127.0.0.1:{}/live", l.local_addr().unwrap().port());
        let incoming = connections(l);
        let mut p = small_profile();
        p.output.folder = dir.to_string_lossy().into_owned();
        p.audio_tracks.enabled = true;
        p.audio_tracks.when = AudioTracksWhen::Streaming;
        p.audio_tracks.buffer_mib = 16;
        p.stream.key = "profile-key".into();
        p.stream.save_copy = true;
        p.stream.destinations = vec![Destination { id: "a".into(), server: url, key: KeyRef::Profile, ..Destination::default() }];
        let mut s = studio_with(Some(noop_gpu()), SceneCollection::new("Test", "Scene"), p);
        s.run("source.add", &json!({"kind": "audio_input", "name": "Mic"})).unwrap();
        s.run("source.add", &json!({"kind": "audio_output", "name": "Desk"})).unwrap();
        s.start_output(OutputKind::Streaming).unwrap();
        let conn = incoming.recv_timeout(GUARD).expect("the stream connects");
        let copy = s.outputs.stream_copy().expect("the stream has a copy").path;
        let stem = copy.file_stem().unwrap().to_string_lossy().into_owned();
        assert!(stem.starts_with("Stream ") && stem.ends_with(" (2)"), "{stem}");
        armed(&mut s);
        assert!(dir.join(format!("{stem} Audio")).is_dir(), "the take has the copy's name");
        drop(conn);
        s.stop_output(OutputKind::Streaming);
        while s.output(OutputKind::Streaming).is_some() {
            s.tick();
            std::thread::yield_now();
        }
        s.finish_audio_tracks_blocking();
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_take_handed_to_an_output_that_fails_opens_when_nothing_follows_it() {
        let (mut s, dir) = studio("handoff-fails", true);
        let fake = Arc::new(crate::audio_editor::tests::Fake::default());
        s.audio_tracks.launcher = Some(fake.clone());
        let mut p = s.profile().clone();
        p.audio_tracks.when = AudioTracksWhen::Either;
        p.audio_tracks.after_stop = studiocraft_model::AfterStop::OpenEditor;
        s.set_profile(p).unwrap();
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        // A recording starts and hands the stream's take on; the stream stops before the
        // recording fails, so no take continues the stream's.
        let rec = dir.join("2026-11-18 22-00-00.mkv");
        let now = s.clock.now();
        s.audio_tracks_output_starting(OutputKind::Recording, now, Some(&rec));
        s.test_streaming = false;
        s.audio_tracks_output_failed(OutputKind::Recording);
        assert!(s.audio_tracks_take().is_none(), "nothing follows the stream's take");
        let mut spins = 0u64;
        while !s.audio_tracks.finishing.is_empty() {
            spins += 1;
            assert!(spins < 200_000_000, "the takes never finished");
            s.tick();
            std::thread::yield_now();
        }
        s.audio_tracks_opened();
        let session = s.state_json()["audio_tracks"]["last_session"].as_str().map(std::path::PathBuf::from).unwrap();
        let folder = session.parent().unwrap().to_path_buf();
        assert_eq!(*fake.log.lock().unwrap(), [format!("folder {}", folder.display())], "the stream's take opens as the last one");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn audio_tracks_status_and_repair_are_commands() {
        let (mut s, dir) = studio("cmd", false);
        let (v, _) = s.run("audio_tracks.status", &json!({})).unwrap();
        assert_eq!((v["enabled"].clone(), v["buffer_mib"].clone()), (json!(false), json!(16)));
        std::fs::create_dir_all(&dir).unwrap();
        // The repair runs on its own thread and the command returns at once. The gate holds the
        // repair until the status has been read, so the command's own tick cannot take it first.
        let (go, wait) = std::sync::mpsc::channel();
        s.audio_tracks.repair_gate = Some(wait);
        let (v, _) = s.run("audio_tracks.repair", &json!({"folder": dir.to_string_lossy()})).unwrap();
        assert_eq!(v["started"], json!(true));
        assert_eq!(s.state_json()["audio_tracks"]["repairing"], json!(true));
        let _ = go.send(());
        assert_eq!(s.audio_tracks_repair_result(), Some(json!({"repaired": [], "failed": []})));
        assert_eq!(s.state_json()["audio_tracks"]["repairing"], json!(false));
        assert_eq!(s.state_json()["audio_tracks"]["last_repair"], json!({"repaired": [], "failed": []}));
        let missing = s.run("audio_tracks.repair", &json!({"folder": dir.join("missing").to_string_lossy()}));
        assert!(missing.is_err(), "a folder that is not there is an error");
        assert_eq!(s.state_json()["audio_tracks"]["repairing"], json!(false), "and starts no repair");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_take_that_ends_opens_its_session_and_a_missing_editor_shows_its_folder() {
        let (mut s, dir) = studio("after-stop", true);
        let fake = Arc::new(crate::audio_editor::tests::Fake::default());
        s.audio_tracks.launcher = Some(fake.clone());
        let mut p = s.profile().clone();
        p.audio_tracks.after_stop = studiocraft_model::AfterStop::OpenEditor;
        s.set_profile(p).unwrap();
        let none = s.run("audio_tracks.open_last", &json!({})).unwrap_err().to_string();
        assert!(none.contains("no take of audio tracks has ended"), "{none}");
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        s.test_streaming = false;
        s.tick();
        let mut spins = 0u64;
        while !s.audio_tracks.finishing.is_empty() {
            spins += 1;
            assert!(spins < 200_000_000, "the take never finished");
            s.tick();
            std::thread::yield_now();
        }
        s.audio_tracks_opened();
        let session = s.state_json()["audio_tracks"]["last_session"].as_str().map(std::path::PathBuf::from).unwrap();
        assert!(session.is_file() && session.extension().is_some_and(|e| e == "scraft"), "{}", session.display());
        let folder = session.parent().unwrap().to_path_buf();
        assert_eq!(*fake.log.lock().unwrap(), [format!("folder {}", folder.display())], "the editor is not on the PATH, so the folder opens");
        let notices: Vec<String> = s.take_notices().into_iter().map(|n| n.text()).collect();
        assert!(notices.iter().any(|n| n.starts_with("The audio editor did not start") && n.contains(&folder.display().to_string())), "{notices:?}");
        // The command opens the same session again, and returns before the opening ends.
        let (v, _) = s.run("audio_tracks.open_last", &json!({})).unwrap();
        assert_eq!(v["session"], json!(session.display().to_string()));
        s.audio_tracks_opened();
        assert_eq!(fake.log.lock().unwrap().len(), 2);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_marker_set_during_a_take_is_in_its_session() {
        let (mut s, dir) = studio("take-marker", true);
        let none = s.run("audio_tracks.marker", &json!({})).unwrap_err().to_string();
        assert!(none.contains("no take of audio tracks is running"), "{none}");
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        let (v, _) = s.run("audio_tracks.marker", &json!({"name": "Goal"})).unwrap();
        assert_eq!(v["name"], json!("Goal"));
        assert!(v["seconds"].as_f64().is_some_and(|t| t >= 0.0), "{v}");
        let (v, _) = s.run("audio_tracks.marker", &json!({})).unwrap();
        assert_eq!(v["name"], json!("Marker 2"), "an unnamed marker is numbered in the take");
        s.test_streaming = false;
        s.tick();
        let mut spins = 0u64;
        while !s.audio_tracks.finishing.is_empty() {
            spins += 1;
            assert!(spins < 200_000_000, "the take never finished");
            s.tick();
            std::thread::yield_now();
        }
        let session = s.state_json()["audio_tracks"]["last_session"].as_str().map(std::path::PathBuf::from).unwrap();
        let text = std::fs::read_to_string(&session).unwrap();
        assert!(text.contains("\"Goal\"") && text.contains("\"Marker 2\""), "{text}");
        let after = s.run("audio_tracks.marker", &json!({})).unwrap_err().to_string();
        assert!(after.contains("no take of audio tracks is running"), "{after}");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_marker_goes_to_the_take_and_is_listed() {
        let (mut s, dir) = studio("session-marker", true);
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        let (v, _) = s.run("marker.add", &json!({"name": "Goal"})).unwrap();
        assert_eq!((v["index"].clone(), v["name"].clone()), (json!(1), json!("Goal")));
        assert_eq!(v["recording"], json!(null), "no recording runs");
        assert!(v["audio_tracks"]["seconds"].as_f64().is_some_and(|t| t >= 0.0), "{v}");
        let (v, _) = s.run("marker.add", &json!({})).unwrap();
        assert_eq!(v["name"], json!("Marker 2"), "an unnamed marker is numbered in the session");
        let (list, _) = s.run("marker.list", &json!({})).unwrap();
        assert_eq!(list["running"], json!(true));
        let listed: Vec<&str> = list["markers"].as_array().unwrap().iter().map(|m| m["name"].as_str().unwrap()).collect();
        assert_eq!(listed, ["Goal", "Marker 2"]);
        s.test_streaming = false;
        s.tick();
        let mut spins = 0u64;
        while !s.audio_tracks.finishing.is_empty() {
            spins += 1;
            assert!(spins < 200_000_000, "the take never finished");
            s.tick();
            std::thread::yield_now();
        }
        let session = s.state_json()["audio_tracks"]["last_session"].as_str().map(std::path::PathBuf::from).unwrap();
        let text = std::fs::read_to_string(&session).unwrap();
        assert!(text.contains("\"Goal\"") && text.contains("\"Marker 2\""), "{text}");
        // The ended session's markers stay listed until the next session starts.
        let (list, _) = s.run("marker.list", &json!({})).unwrap();
        assert_eq!(list["running"], json!(false));
        assert_eq!(list["markers"].as_array().unwrap().len(), 2);
        let e = s.run("marker.add", &json!({})).unwrap_err().to_string();
        assert_eq!(e, "no recording, stream copy or take of audio tracks is running");
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        let (list, _) = s.run("marker.list", &json!({})).unwrap();
        assert_eq!(list, json!({"running": true, "markers": []}), "a new take starts a new session");
        s.test_streaming = false;
        s.tick();
        let mut spins = 0u64;
        while !s.audio_tracks.finishing.is_empty() {
            spins += 1;
            assert!(spins < 200_000_000, "the second take never finished");
            s.tick();
            std::thread::yield_now();
        }
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_marker_is_renamed_in_the_take_while_it_runs_and_after_it_ended() {
        let (mut s, dir) = studio("session-marker-rename", true);
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        s.run("marker.add", &json!({"name": "Goal"})).unwrap();
        s.run("marker.add", &json!({"name": "Save"})).unwrap();
        let (v, _) = s.run("marker.rename", &json!({"index": 1, "name": "Kickoff"})).unwrap();
        assert_eq!((v["index"].clone(), v["name"].clone(), v["recording"].clone()), (json!(1), json!("Kickoff"), json!(null)));
        assert!(v["audio_tracks"]["seconds"].as_f64().is_some(), "{v}");
        let e = s.run("marker.rename", &json!({"index": 3, "name": "Late"})).unwrap_err().to_string();
        assert_eq!(e, "there is no marker 3: the session holds markers 1 to 2");
        let (v, _) = s.run("marker.rename", &json!({"index": 2, "name": "Save"})).unwrap();
        assert!(v["audio_tracks"]["seconds"].as_f64().is_some(), "a running take keeps the name it has: {v}");
        s.test_streaming = false;
        s.tick();
        let mut spins = 0u64;
        while !s.audio_tracks.finishing.is_empty() {
            spins += 1;
            assert!(spins < 200_000_000, "the take never finished");
            s.tick();
            std::thread::yield_now();
        }
        let session = s.state_json()["audio_tracks"]["last_session"].as_str().map(std::path::PathBuf::from).unwrap();
        let names = |p: &std::path::Path| {
            let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap();
            v["markers"].as_array().unwrap().iter().map(|m| m["name"].as_str().unwrap().to_string()).collect::<Vec<_>>()
        };
        assert_eq!(names(&session), ["Kickoff", "Save"], "the session the take wrote has the new name");
        // After the take, the session file is written again with the new name.
        let (v, _) = s.run("marker.rename", &json!({"index": 2, "name": "Final save"})).unwrap();
        assert_eq!(v["name"], json!("Final save"));
        assert_eq!(names(&session), ["Kickoff", "Final save"]);
        let (list, _) = s.run("marker.list", &json!({})).unwrap();
        let listed: Vec<&str> = list["markers"].as_array().unwrap().iter().map(|m| m["name"].as_str().unwrap()).collect();
        assert_eq!(listed, ["Kickoff", "Final save"]);
        // The marker was renamed in the audio editor: the take reports it, and the marker keeps
        // its name.
        let text = std::fs::read_to_string(&session).unwrap().replace("\"Final save\"", "\"Edited\"");
        std::fs::write(&session, text).unwrap();
        let e = s.run("marker.rename", &json!({"index": 2, "name": "Again"})).unwrap_err().to_string();
        assert!(e.starts_with("the marker was not renamed (the audio tracks: the session file could not be written again"), "{e}");
        let (list, _) = s.run("marker.list", &json!({})).unwrap();
        assert_eq!(list["markers"][1]["name"], json!("Final save"), "a rename no output made keeps the old name");
        // A rename to the name the marker has is checked against the file too.
        let edited = std::fs::read(&session).unwrap();
        let e = s.run("marker.rename", &json!({"index": 2, "name": "Final save"})).unwrap_err().to_string();
        assert!(e.starts_with("the marker was not renamed (the audio tracks: the session file could not be written again"), "{e}");
        assert_eq!(std::fs::read(&session).unwrap(), edited, "the edited file is left as it is");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A studio whose takes show their folder when they end, through `fake`.
    fn showing_folders(name: &str, fake: &Arc<crate::audio_editor::tests::Fake>) -> (Studio, std::path::PathBuf) {
        let (mut s, dir) = studio(name, true);
        s.audio_tracks.launcher = Some(fake.clone());
        let mut p = s.profile().clone();
        p.audio_tracks.after_stop = studiocraft_model::AfterStop::ShowFolder;
        s.set_profile(p).unwrap();
        (s, dir)
    }

    #[test]
    fn takes_that_end_between_two_ticks_each_show_their_folder() {
        let fake = Arc::new(crate::audio_editor::tests::Fake::default());
        let (mut s, dir) = showing_folders("after-stop-two", &fake);
        // The first take is held back from the studio while the second runs, so no tick sees it
        // finish before the second ends.
        let mut held = None;
        for _ in 0..2 {
            s.test_streaming = true;
            s.tick();
            armed(&mut s);
            s.test_streaming = false;
            s.tick();
            if held.is_none() {
                held = Some(s.audio_tracks.finishing.remove(0));
            }
        }
        s.audio_tracks.finishing.extend(held);
        s.audio_tracks.finishing.swap(0, 1);
        assert_eq!(s.audio_tracks.finishing.len(), 2, "both takes are still finishing");
        // Both takes finish before the next tick looks at them; the mixer's handed-back tap sets
        // are freed here as a tick would.
        let mut spins = 0u64;
        while !s.audio_tracks.finishing.iter().all(|f| f.0.finished()) {
            spins += 1;
            assert!(spins < 200_000_000, "the takes never finished");
            s.mixer.free_retired();
            std::thread::yield_now();
        }
        // The newer take is looked at first, as when it finishes writing before the older one:
        // the last session is still the newer take's.
        let newer = s.audio_tracks.finishing[1].0.status().session().map(std::path::Path::to_path_buf);
        assert!(newer.is_some());
        s.audio_tracks.finishing.swap(0, 1);
        s.tick();
        s.audio_tracks_opened();
        let log = fake.log.lock().unwrap().clone();
        assert_eq!(log.len(), 2, "{log:?}");
        assert_eq!(s.audio_tracks_last_session().map(std::path::Path::to_path_buf), newer);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_older_take_that_finishes_after_a_newer_one_opens_nothing() {
        let fake = Arc::new(crate::audio_editor::tests::Fake::default());
        let (mut s, dir) = showing_folders("after-stop-late", &fake);
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        s.test_streaming = false;
        s.tick();
        // The older take is held back from the studio until the newer one has been seen, as when
        // its files take longer to finish. The tick that stopped it has not looked at it yet.
        let older = s.audio_tracks.finishing.remove(0);
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        s.test_streaming = false;
        s.tick();
        let mut spins = 0u64;
        while !(older.0.finished() && s.audio_tracks.finishing.iter().all(|f| f.0.finished())) {
            spins += 1;
            assert!(spins < 200_000_000, "the takes never finished");
            s.mixer.free_retired();
            std::thread::yield_now();
        }
        let newer = s.audio_tracks.finishing[0].0.status().session().map(std::path::Path::to_path_buf);
        s.tick();
        s.audio_tracks.finishing.push(older);
        s.tick();
        s.audio_tracks_opened();
        let log = fake.log.lock().unwrap().clone();
        assert_eq!(log.len(), 1, "only the newer take opens: {log:?}");
        assert_eq!(s.audio_tracks_last_session().map(std::path::Path::to_path_buf), newer);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Start a repair that writes `session` once `go` is sent, as a repair that takes a while.
    fn held_repair(s: &mut Studio, session: &std::path::Path) -> std::sync::mpsc::Sender<()> {
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let session = session.to_path_buf();
        let h = std::thread::spawn(move || {
            let _ = wait.recv();
            let dir = session.with_file_name("Audio Files");
            let repaired = studiocraft_audio_tracks::recover::Repaired { dir, files: Vec::new(), synced: Vec::new(), session: Some(session) };
            studiocraft_audio_tracks::recover::Outcome { repaired: vec![repaired], failed: Vec::new() }
        });
        s.push_repair(h);
        go
    }

    /// Run the repairs that are ready to end and take their results.
    fn end_repairs(s: &mut Studio, go: std::sync::mpsc::Sender<()>) {
        let _ = go.send(());
        let _ = s.audio_tracks_repair_result();
    }

    #[test]
    fn a_repair_that_finishes_after_a_newer_take_keeps_that_take_as_the_last() {
        let fake = Arc::new(crate::audio_editor::tests::Fake::default());
        let (mut s, dir) = showing_folders("after-stop-repair", &fake);
        let old = dir.join("Old Audio").join("Old.scraft");
        let go = held_repair(&mut s, &old);
        // A take starts and ends while the repair runs.
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        s.test_streaming = false;
        s.tick();
        let mut spins = 0u64;
        while !s.audio_tracks.finishing.is_empty() {
            spins += 1;
            assert!(spins < 200_000_000, "the take never finished");
            s.tick();
            std::thread::yield_now();
        }
        let take = s.audio_tracks_last_session().map(std::path::Path::to_path_buf);
        assert!(take.is_some());
        end_repairs(&mut s, go);
        assert_eq!(s.audio_tracks_last_session().map(std::path::Path::to_path_buf), take, "the repair started before the take stopped");
        // A repair started after that replaces it.
        let newer = dir.join("Newer Audio").join("Newer.scraft");
        let go = held_repair(&mut s, &newer);
        end_repairs(&mut s, go);
        assert_eq!(s.audio_tracks_last_session(), Some(newer.as_path()));
        // A take that stopped before a repair started still opens when it ends after the repair,
        // and the repair's session stays the last one.
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        s.test_streaming = false;
        s.tick();
        let held = s.audio_tracks.finishing.remove(0);
        let later = dir.join("Later Audio").join("Later.scraft");
        let go = held_repair(&mut s, &later);
        end_repairs(&mut s, go);
        s.audio_tracks.finishing.push(held);
        let mut spins = 0u64;
        while !s.audio_tracks.finishing.is_empty() {
            spins += 1;
            assert!(spins < 200_000_000, "the take never finished");
            s.tick();
            std::thread::yield_now();
        }
        s.audio_tracks_opened();
        assert_eq!(fake.log.lock().unwrap().len(), 2, "each take opens its folder");
        assert_eq!(s.audio_tracks_last_session(), Some(later.as_path()));
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_take_that_ends_for_the_next_one_opens_nothing_when_both_end_between_two_ticks() {
        let fake = Arc::new(crate::audio_editor::tests::Fake::default());
        let (mut s, dir) = showing_folders("after-stop-next", &fake);
        let mut p = s.profile().clone();
        p.audio_tracks.when = AudioTracksWhen::Either;
        s.set_profile(p).unwrap();
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        // A recording starts during the stream: the stream's take ends for the recording's.
        let rec = dir.join("2026-11-20 09-00-00.mkv");
        let now = s.clock.now();
        s.audio_tracks_output_starting(OutputKind::Recording, now, Some(&rec));
        s.test_streaming = false;
        s.stop_audio_tracks();
        assert_eq!(s.audio_tracks.finishing.len(), 2, "both takes are still finishing");
        let mut spins = 0u64;
        while !s.audio_tracks.finishing.iter().all(|f| f.0.finished()) {
            spins += 1;
            assert!(spins < 200_000_000, "the takes never finished");
            s.mixer.free_retired();
            std::thread::yield_now();
        }
        s.tick();
        s.audio_tracks_opened();
        let log = fake.log.lock().unwrap().clone();
        let folder = dir.join("2026-11-20 09-00-00 Audio");
        assert_eq!(log, [format!("folder {}", folder.display())], "only the recording's take opens");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_take_running_when_the_app_closes_shows_its_folder() {
        let fake = Arc::new(crate::audio_editor::tests::Fake::default());
        let (mut s, dir) = showing_folders("after-stop-close", &fake);
        s.test_streaming = true;
        s.tick();
        armed(&mut s);
        drop(s);
        let log = fake.log.lock().unwrap().clone();
        assert_eq!(log.len(), 1, "{log:?}");
        assert!(log[0].starts_with("folder "), "{log:?}");
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
    use studiocraft_plugin::{SourceIo, VideoContent};

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
        headless()
    }

    #[test]
    fn each_studio_shows_only_the_captions_it_heard() {
        let (a, b) = (headless(), headless());
        assert!(!Arc::ptr_eq(&a.caption_board(), &b.caption_board()));
        a.caption_board().push_final("only for the first studio", Tick::from_units(0, 1000));
        let source = b.registry().source("captions").unwrap();
        let io = SourceIo::new(MasterClock::new());
        let mut i = source.create(&source.schema().defaults().with("clear_after", 0), &io).unwrap();
        i.produce(&io).unwrap();
        assert!(matches!(io.video.take(), Some(VideoContent::Empty)), "the second studio's source shows nothing");
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

mod pause_screen {
    use super::*;

    /// The ids of the sources a plan draws in the program, bottom to top.
    fn program_sources(s: &mut Studio) -> Vec<u64> {
        s.plan().program.items.iter().map(|i| i.source.0).collect()
    }

    /// The ids of the sources the running transition starts from.
    fn transition_from(s: &mut Studio) -> Vec<u64> {
        s.plan().transition.as_ref().map(|t| t.from.items.iter().map(|i| i.source.0).collect()).unwrap_or_default()
    }

    /// The pause screen's background draws under this id while no source holds it.
    const BACKGROUND: u64 = u64::MAX;

    fn with_a_scene_source() -> (Studio, u64) {
        let mut s = headless();
        let (out, _) = s.run("source.add", &json!({"kind": "color", "name": "Card"})).unwrap();
        let id = out["source"].as_u64().unwrap_or_else(|| panic!("{out}"));
        s.tick();
        (s, id)
    }

    #[test]
    fn a_pause_shows_the_pause_screen_and_resuming_shows_the_scene() {
        let (mut s, card) = with_a_scene_source();
        s.transition = None;
        assert_eq!(program_sources(&mut s), [card]);
        let rev = s.engine().revision();
        let dirty = s.engine().is_dirty();
        let (status, _) = s.run("pause.start", &json!({"countdown_s": 90})).unwrap();
        assert_eq!(status["paused"], json!(true));
        assert_eq!(status["countdown"]["total_s"], json!(90));
        assert_eq!(program_sources(&mut s).first(), Some(&BACKGROUND), "the background is drawn first");
        assert_eq!(transition_from(&mut s), [card], "the collection's fade runs from the scene into the pause screen");
        assert_eq!((s.engine().revision(), s.engine().is_dirty()), (rev, dirty), "a pause changes nothing in the collection");
        assert_eq!(s.state_json()["pause"]["paused"], json!(true));
        s.run("pause.stop", &json!({})).unwrap();
        assert_eq!(program_sources(&mut s), [card]);
        assert_eq!(transition_from(&mut s).first(), Some(&BACKGROUND), "the fade back runs from the pause screen");
        assert!(s.pause_layers.is_some(), "the pause screen keeps drawing until the fade has ended");
        assert_eq!(s.run("pause.status", &json!({})).unwrap().0, json!({"paused": false}));
        let notices: Vec<&str> = s.take_notices().iter().map(|n| n.template).collect();
        assert!(notices.contains(&"The program is paused") && notices.contains(&"The program resumed"), "{notices:?}");
        // Once the fade has ended, the pause screen's threads stop.
        s.transition = None;
        s.tick();
        assert!(s.pause_layers.is_none());
    }

    #[test]
    fn resuming_or_adding_time_while_not_paused_is_an_error() {
        let mut s = headless();
        assert!(s.run("pause.stop", &json!({})).is_err());
        assert!(s.run("pause.extend", &json!({"seconds": 60})).is_err());
        assert!(s.pause.is_none());
    }

    #[test]
    fn toggling_pauses_with_the_profile_settings_and_resumes() {
        let mut profile = Profile::default();
        profile.pause.countdown_s = 0;
        profile.pause.mute_scene = false;
        let mut s = studio_with(None, SceneCollection::new("Test", "Scene"), profile);
        let (status, _) = s.run("pause.toggle", &json!({})).unwrap();
        assert_eq!((status["paused"].clone(), status["countdown"].clone()), (json!(true), Value::Null), "no countdown: the profile's 0");
        let (status, _) = s.run("pause.toggle", &json!({})).unwrap();
        assert_eq!(status, json!({"paused": false}), "resuming reports the status too");
        assert!(s.pause.is_none());
    }

    #[test]
    fn settings_with_a_countdown_text_without_its_time_are_refused() {
        let mut s = headless();
        let mut p = s.profile().clone();
        p.pause.text = "Back soon".into();
        assert!(s.set_profile(p).is_err());
        assert_ne!(s.profile().pause.text, "Back soon", "the old settings stay");
    }

    #[test]
    fn a_file_the_screen_cannot_show_is_an_error_and_does_not_pause() {
        let mut s = headless();
        for bad in ["song.mp3", "notes.txt", "noextension"] {
            assert!(s.run("pause.start", &json!({"media": bad})).is_err(), "{bad}");
        }
        assert!(s.pause.is_none());
        // While paused, a bad file is refused and the pause keeps its media.
        s.run("pause.start", &json!({"media": ""})).unwrap();
        assert!(s.run("pause.start", &json!({"media": "x.doc"})).is_err());
        assert_eq!(s.pause.as_ref().map(|p| p.settings.media.as_str()), Some(""));
    }

    #[test]
    fn values_given_while_paused_apply_without_a_new_transition() {
        let mut s = headless();
        s.run("transition.set", &json!({"kind": "cut"})).unwrap();
        s.transition = None;
        s.run("pause.start", &json!({"countdown_s": 600})).unwrap();
        assert!(s.transition.is_none(), "a cut shows the pause screen at once");
        s.run("transition.set", &json!({"kind": "fade"})).unwrap();
        let (status, _) = s.run("pause.start", &json!({"countdown_s": 30, "at_zero": "resume"})).unwrap();
        assert!(s.transition.is_none(), "no new transition while paused");
        assert_eq!(status["countdown"]["total_s"], json!(30));
        assert_eq!(status["countdown"]["at_zero"], json!("resume"));
        let (status, _) = s.run("pause.extend", &json!({"seconds": 60})).unwrap();
        assert_eq!(status["countdown"]["total_s"], json!(90));
    }

    #[test]
    fn every_pause_screen_setting_can_be_given_for_one_pause() {
        use studiocraft_model::PauseAnchor;
        let mut s = headless();
        s.run("transition.set", &json!({"kind": "cut"})).unwrap();
        s.transition = None;
        let before = s.profile().pause.clone();
        assert!(before.mute_scene, "the default profile holds the scene's audio");
        let given = json!({
            "text": "On air in {time}", "zero_text": "Starting now", "anchor": "top_left", "size_pct": 12,
            "fit": "fill", "background": "#20304080", "mute_scene": false
        });
        s.run("pause.start", &given).unwrap();
        let Some(p) = &s.pause else { panic!("the program is paused") };
        let set = p.settings.clone();
        assert_eq!((set.text.as_str(), set.zero_text.as_str()), ("On air in {time}", "Starting now"));
        assert_eq!((set.anchor, set.size_pct, set.fit, set.background.as_str()), (PauseAnchor::TopLeft, 12, FitMode::Fill, "#20304080"));
        assert!(!set.mute_scene);
        assert_eq!(s.pause_layers.as_ref().map(|l| l.look.clone()), Some(set), "the screen is drawn with the given values");
        assert!(s.hold_fade.is_none(), "the scene's audio is not held");
        assert_eq!(s.profile().pause, before, "the profile keeps its settings");

        // While paused, a given value applies at once, and the scene's audio fades out over the
        // collection's transition length.
        s.run("transition.set", &json!({"kind": "fade"})).unwrap();
        let ms = s.engine().collection().transition.duration_ms;
        assert!(ms > 0);
        s.run("pause.start", &json!({"mute_scene": true, "anchor": "center"})).unwrap();
        assert!(s.transition.is_none(), "no new transition while paused");
        let Some(h) = s.hold_fade else { panic!("the scene's audio fades out") };
        assert!(h.down && h.ms == ms, "{h:?}");
        assert_eq!(s.pause_layers.as_ref().map(|l| l.look.anchor), Some(PauseAnchor::Center));
        assert_eq!(s.pause.as_ref().map(|p| p.settings.text.as_str()), Some("On air in {time}"), "the values not given stay");
        // The value the pause already has starts no new fade.
        s.run("pause.start", &json!({"mute_scene": true})).unwrap();
        assert_eq!(s.hold_fade.map(|h| h.start), Some(h.start));
        s.run("pause.stop", &json!({})).unwrap();
        assert!(s.hold_fade.is_some_and(|h| !h.down), "resuming brings the scene's audio back");
        assert_eq!(s.profile().pause, before);
    }

    #[test]
    fn a_scene_switch_while_paused_changes_what_resuming_shows() {
        let (mut s, _) = with_a_scene_source();
        s.run("pause.start", &json!({})).unwrap();
        s.transition = None;
        s.run("scene.add", &json!({"name": "After the break"})).unwrap();
        assert!(s.transition.is_none(), "the scene changes under the pause screen");
        assert_eq!(program_sources(&mut s).first(), Some(&BACKGROUND));
        let second = s.collection().program_scene;
        s.run("pause.stop", &json!({})).unwrap();
        assert_eq!(s.shown_program, second, "resuming shows the new scene");
        assert!(program_sources(&mut s).is_empty(), "the new scene is empty");
    }

    #[test]
    fn pausing_during_the_fade_back_turns_the_fade_around_where_it_stands() {
        let mut s = headless();
        s.run("pause.start", &json!({})).unwrap();
        s.transition = None;
        s.run("pause.stop", &json!({})).unwrap();
        // The fade back is held at its very start, so turning it around leaves nothing to run.
        let Some(t) = &mut s.transition else { panic!("the collection's fade runs") };
        t.start = Tick(s.clock.now().0.saturating_add(10_000_000_000));
        s.run("pause.start", &json!({})).unwrap();
        let Some(t) = &s.transition else { panic!("a fade runs into the pause screen") };
        assert!(matches!(t.from, Origin::Scene(_)));
        assert!(t.start.0.saturating_add(t.duration.0) <= s.clock.now().0, "the new fade starts where the old one stood");
    }

    #[test]
    fn a_swipe_starts_over_instead_of_turning_around() {
        let mut s = headless();
        s.run("transition.set", &json!({"kind": "swipe"})).unwrap();
        s.run("pause.start", &json!({})).unwrap();
        s.transition = None;
        s.run("pause.stop", &json!({})).unwrap();
        let Some(t) = &mut s.transition else { panic!("the collection's swipe runs") };
        t.start = Tick(s.clock.now().0.saturating_add(10_000_000_000));
        let before = s.clock.now().0;
        s.run("pause.start", &json!({})).unwrap();
        let Some(t) = &s.transition else { panic!("a swipe runs into the pause screen") };
        assert!(t.start.0 >= before, "the new swipe starts from the beginning");
    }

    #[test]
    fn pausing_during_the_fade_back_keeps_the_layers_that_draw() {
        let mut s = headless();
        s.run("pause.start", &json!({})).unwrap();
        let before = s.plan().program.items[0].video.clone();
        s.run("pause.stop", &json!({})).unwrap();
        assert!(s.transition.is_some(), "the fade back runs");
        s.run("pause.start", &json!({})).unwrap();
        let after = s.plan().program.items[0].video.clone();
        assert!(Arc::ptr_eq(&before, &after), "the background's thread was kept");
    }

    #[test]
    fn an_eased_fade_turns_around_at_the_blend_it_shows() {
        let mut s = headless();
        s.run("transition.set", &json!({"kind": "fade"})).unwrap();
        s.run("transition.settings", &json!({"settings": {"fade_easing": "ease_in"}})).unwrap();
        s.run("pause.start", &json!({})).unwrap();
        s.transition = None;
        s.run("pause.stop", &json!({})).unwrap();
        // A quarter of the fade back's time has passed, where ease in shows 1/16 of the scene.
        let Some(t) = &mut s.transition else { panic!("the collection's fade runs") };
        t.start = Tick(s.clock.now().0.saturating_sub(t.duration.0 / 4));
        s.run("pause.start", &json!({})).unwrap();
        let Some(t) = &s.transition else { panic!("a fade runs into the pause screen") };
        // The new fade shows 15/16 of the pause screen, which ease in reaches at sqrt(15)/4 of
        // its time (0.968); turning the time around would give 0.75.
        let along = s.clock.now().0.saturating_sub(t.start.0) as f64 / t.duration.0 as f64;
        assert!(along > 0.9, "{along}");
    }

    #[test]
    fn the_progress_showing_a_blend_inverts_the_ease() {
        let quad_in = |x: f64| x * x;
        assert!((progress_showing(quad_in, 0.0625) - 0.25).abs() < 1e-9);
        assert!((progress_showing(|x| x, 0.4) - 0.4).abs() < 1e-9);
        assert!(progress_showing(quad_in, 0.0) < 1e-9, "at the start");
        assert!(progress_showing(|_| 0.0, 0.5) > 0.999_999, "a blend that never gets there gives the end");
    }

    #[test]
    fn a_turned_around_fade_ends_the_audio_fade_with_the_picture() {
        // Linear: halfway back takes half the time, at the full fade's rate.
        assert_eq!(turned_fade_ms(1000, 0.5, 0.5), 1000);
        // Ease in, turned around a quarter of the way: the picture has 1 - sqrt(15)/4 of its time
        // left, and the audio a quarter of its range to go back in that time.
        let left = 1.0 - 15f64.sqrt() / 4.0;
        assert_eq!(turned_fade_ms(1000, 0.25, left), (1000.0 * left / 0.25).round() as u32);
        assert!(turned_fade_ms(1000, 0.25, left) < 200, "the audio does not run on after the picture");
        assert_eq!(turned_fade_ms(1000, 0.0, 0.3), 1000, "audio with nothing to move keeps the length for the scene sources' fades");
        assert_eq!(turned_fade_ms(u32::MAX, 1e-12, 1.0), u32::MAX, "no overflow");
    }

    #[test]
    fn the_program_audio_ends_its_fade_with_the_picture_after_each_turn() {
        let mut s = headless();
        s.run("transition.set", &json!({"kind": "fade"})).unwrap();
        s.run("transition.settings", &json!({"settings": {"fade_easing": "ease_out"}})).unwrap();
        s.run("pause.start", &json!({})).unwrap();
        for turn in 0..4 {
            // A third of the running fade's time passes, for the picture and for the audio.
            let (Some(t), Some(h)) = (&mut s.transition, &mut s.hold_fade) else { panic!("turn {turn}: a fade runs with the audio") };
            let third = t.duration.0 / 3;
            t.start = Tick(t.start.0.saturating_sub(third));
            h.start = Tick(h.start.0.saturating_sub(third));
            let id = if s.pause.is_some() { "pause.stop" } else { "pause.start" };
            s.run(id, &json!({})).unwrap();
            let now = s.clock.now();
            let (Some(t), Some(h)) = (&s.transition, &s.hold_fade) else { panic!("turn {turn}: the fade was turned around") };
            let picture_left = duration_of(Tick(t.start.0.saturating_add(t.duration.0).saturating_sub(now.0))).as_secs_f64() * 1000.0;
            let audio_left = (if h.down { h.from } else { 1.0 - h.from }) * f64::from(h.ms);
            assert!((picture_left - audio_left).abs() < 5.0, "turn {turn}: the picture has {picture_left} ms left, the audio {audio_left} ms");
        }
    }

    #[test]
    fn a_scene_switch_during_the_fade_back_keeps_the_fade_from_the_pause_screen() {
        let (mut s, _) = with_a_scene_source();
        s.run("pause.start", &json!({})).unwrap();
        s.transition = None;
        s.run("pause.stop", &json!({})).unwrap();
        assert_eq!(transition_from(&mut s).first(), Some(&BACKGROUND), "the fade back runs");
        s.run("scene.add", &json!({"name": "After the break"})).unwrap();
        let second = s.collection().program_scene;
        assert_eq!(transition_from(&mut s).first(), Some(&BACKGROUND), "the fade still runs from the pause screen");
        assert_eq!(s.shown_program, second, "and ends on the new scene");
        assert!(program_sources(&mut s).is_empty(), "the new scene is empty");
    }

    #[test]
    fn the_countdown_at_zero_resumes_once_when_set_to_resume() {
        let mut s = headless();
        s.run("pause.start", &json!({"countdown_s": 300, "at_zero": "resume"})).unwrap();
        s.take_notices();
        // The countdown's end is moved to the start of the clock instead of waiting for it.
        if let Some(p) = &mut s.pause {
            p.deadline_ms = Some(0);
        }
        s.tick();
        assert!(s.pause.is_none(), "zero resumed the program");
        s.tick();
        let notices: Vec<&str> = s.take_notices().iter().map(|n| n.template).collect();
        assert_eq!(notices.iter().filter(|n| **n == "The countdown ended").count(), 1, "{notices:?}");
        assert!(notices.contains(&"The program resumed"), "{notices:?}");
    }

    #[test]
    fn the_countdown_at_zero_keeps_the_screen_with_the_text_at_zero() {
        let mut s = headless();
        s.run("pause.start", &json!({"countdown_s": 300})).unwrap();
        if let Some(p) = &mut s.pause {
            p.deadline_ms = Some(0);
        }
        s.tick();
        s.tick();
        let status = s.pause_json();
        assert_eq!((status["paused"].clone(), status["countdown"]["reached"].clone()), (json!(true), json!(true)));
        let notices: Vec<&str> = s.take_notices().iter().map(|n| n.template).collect();
        assert_eq!(notices.iter().filter(|n| **n == "The countdown ended").count(), 1, "{notices:?}");
        let now = s.now_ms();
        assert_eq!(s.pause.as_ref().and_then(|p| p.text(now)).as_deref(), Some("Back in a moment"));
    }

    #[test]
    fn a_pause_lasts_through_a_new_collection_and_ends_only_on_resume() {
        let mut s = headless();
        s.run("pause.start", &json!({})).unwrap();
        s.load_collection(SceneCollection::new("Other", "Main"));
        s.tick();
        assert!(s.pause.is_some(), "a new collection does not resume the program");
        assert_eq!(program_sources(&mut s).first(), Some(&BACKGROUND));
    }

    /// The picture source. The first instance it creates takes `told` and `go`. When its first
    /// load does not fail, it sends `false` on `told` at once. When it fails, the source thread
    /// stores the error and tries again later; that second load sends `true` and waits on `go`
    /// before it runs, so the test reads the stored error while the thread waits. When that
    /// instance ends first, the receiver sees the channel close.
    struct Telling {
        inner: Arc<dyn studiocraft_plugin::Source>,
        told: std::sync::Mutex<Option<std::sync::mpsc::Sender<bool>>>,
        go: std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    }

    impl studiocraft_plugin::Plugin for Telling {
        fn id(&self) -> &'static str {
            self.inner.id()
        }
        fn name(&self) -> &'static str {
            self.inner.name()
        }
        fn schema(&self) -> studiocraft_model::SettingsSchema {
            self.inner.schema()
        }
    }

    impl studiocraft_plugin::Source for Telling {
        fn caps(&self) -> studiocraft_plugin::SourceCaps {
            self.inner.caps()
        }
        fn create(&self, settings: &Settings, io: &studiocraft_plugin::SourceIo) -> Result<Box<dyn studiocraft_plugin::SourceInstance>, PluginError> {
            let told = self.told.lock().ok().and_then(|mut t| t.take());
            let go = self.go.lock().ok().and_then(|mut g| g.take());
            Ok(Box::new(TellingInstance { inner: self.inner.create(settings, io)?, told, go, loads: 0 }))
        }
    }

    struct TellingInstance {
        inner: Box<dyn studiocraft_plugin::SourceInstance>,
        told: Option<std::sync::mpsc::Sender<bool>>,
        go: Option<std::sync::mpsc::Receiver<()>>,
        loads: u32,
    }

    impl studiocraft_plugin::SourceInstance for TellingInstance {
        fn pace(&self) -> studiocraft_plugin::Pace {
            self.inner.pace()
        }
        fn produce(&mut self, io: &studiocraft_plugin::SourceIo) -> Result<(), PluginError> {
            self.loads += 1;
            if self.loads == 2
                && let Some(tx) = self.told.take()
            {
                // The thread stored the first load's error before this call.
                let _ = tx.send(true);
                if let Some(go) = self.go.take() {
                    let _ = go.recv();
                }
            }
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.inner.produce(io)));
            // A first load that panics is told as one that did not fail, so the test stops.
            if self.loads == 1
                && !matches!(r, Ok(Err(_)))
                && let Some(tx) = self.told.take()
            {
                let _ = tx.send(false);
            }
            r.unwrap_or_else(|p| std::panic::resume_unwind(p))
        }
        fn update(&mut self, settings: &Settings, io: &studiocraft_plugin::SourceIo) -> Result<(), PluginError> {
            self.inner.update(settings, io)
        }
    }

    /// A studio whose picture source is a [`Telling`], with its `told` receiver and `go` sender.
    fn telling_studio() -> (Studio, std::sync::mpsc::Receiver<bool>, std::sync::mpsc::Sender<()>) {
        let ffmpeg: FfmpegSetting = Arc::new(RwLock::new(String::new()));
        let captions = Arc::new(CaptionBoard::new());
        let all = builtin_registry(&ffmpeg, &captions).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel();
        let (told, go) = (std::sync::Mutex::new(Some(tx)), std::sync::Mutex::new(Some(go_rx)));
        let telling = Arc::new(Telling { inner: all.source("image").unwrap(), told, go });
        let mut reg = Registry::new();
        for p in all.sources() {
            if p.id() == "image" {
                reg.register_source(telling.clone()).unwrap();
            } else {
                reg.register_source(p.clone()).unwrap();
            }
        }
        for p in all.filters() {
            reg.register_filter(p.clone()).unwrap();
        }
        for p in all.transitions() {
            reg.register_transition(p.clone()).unwrap();
        }
        let s = Studio::new(None, SceneCollection::new("Test", "Scene"), Profile::default(), Arc::new(reg), ffmpeg, captions);
        (s, rx, go_tx)
    }

    /// Pause on `media` with a minute's countdown, wait for the picture to fail, and check that
    /// the screen still shows its background and countdown and reports the error.
    fn pauses_with_a_media_error(media: &std::path::Path) {
        // The source thread waits before its second load until `_go` is dropped at the end.
        let (mut s, failed, _go) = telling_studio();
        let media = media.to_string_lossy().into_owned();
        let (status, _) = s.run("pause.start", &json!({"media": media, "countdown_s": 60})).unwrap();
        assert_eq!((status["paused"].clone(), status["countdown"]["total_s"].clone()), (json!(true), json!(60)), "{status}");
        assert_eq!(program_sources(&mut s).first(), Some(&BACKGROUND), "the background is up");
        let now = s.now_ms();
        assert_eq!(s.pause.as_ref().and_then(|p| p.text(now)).as_deref(), Some("Back in 1:00"), "the countdown is up");
        let error = match s.pause_json()["media_error"].as_str() {
            // The error was already stored.
            Some(e) => e.to_owned(),
            None => {
                // Once the second load is told, the first load's error is stored and the thread
                // waits, so nothing holds the error while it is read.
                assert_eq!(failed.recv(), Ok(true), "the picture's first load failed");
                s.pause_json()["media_error"].as_str().map(str::to_owned).unwrap_or_else(|| panic!("the status reports no error"))
            }
        };
        assert!(error.contains(&media), "{error}");
        assert_eq!(s.pause_json()["paused"], json!(true), "the pause continues");
        s.tick();
        let notices: Vec<String> = s.take_notices().iter().map(|n| n.template.to_string()).collect();
        assert!(notices.iter().any(|n| n == pause::MEDIA_NOTICE), "{notices:?}");
    }

    #[test]
    fn a_missing_picture_still_pauses_and_reports_the_error() {
        let dir = tempfile::tempdir().unwrap();
        pauses_with_a_media_error(&dir.path().join("missing.png"));
    }

    #[test]
    fn a_picture_wider_than_a_frame_still_pauses_and_reports_the_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.bmp");
        // A 24-bit picture one pixel wider than the 16,384-pixel frame limit, with all its pixels
        // present.
        let (w, h): (u32, u32) = (16_385, 1);
        let row = (w * 3).div_ceil(4) * 4;
        let mut bmp = Vec::new();
        bmp.extend(b"BM");
        bmp.extend((54 + row * h).to_le_bytes());
        bmp.extend(0u32.to_le_bytes());
        bmp.extend(54u32.to_le_bytes());
        bmp.extend(40u32.to_le_bytes());
        bmp.extend(w.to_le_bytes());
        bmp.extend(h.to_le_bytes());
        bmp.extend(1u16.to_le_bytes());
        bmp.extend(24u16.to_le_bytes());
        bmp.extend(0u32.to_le_bytes());
        bmp.extend((row * h).to_le_bytes());
        bmp.extend(2835u32.to_le_bytes());
        bmp.extend(2835u32.to_le_bytes());
        bmp.extend(0u32.to_le_bytes());
        bmp.extend(0u32.to_le_bytes());
        bmp.resize(bmp.len() + (row * h) as usize, 0x80);
        std::fs::write(&path, bmp).unwrap();
        pauses_with_a_media_error(&path);
    }
}
