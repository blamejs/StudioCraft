use std::sync::Arc;

use serde_json::json;
use studiocraft_model::{Field, FieldKind, FitMode, Settings, SettingsSchema, Transform, Vec2};
use studiocraft_plugin::{Filter, FilterInstance, FilterTarget, Plugin, PluginError, Source, SourceCaps, SourceInstance, SourceIo, Transition};

use super::*;
use crate::command::{Command as C, ItemRef, Move, SceneRef, SourceRef};

struct TestSource(&'static str, SourceCaps);
impl Plugin for TestSource {
    fn id(&self) -> &'static str {
        self.0
    }
    fn name(&self) -> &'static str {
        if self.1.video { "Color" } else { "Mic" }
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::new(vec![Field::new("width", "Width", FieldKind::Int { min: 1, max: 100 }, 10)])
    }
}
impl Source for TestSource {
    fn caps(&self) -> SourceCaps {
        self.1
    }
    fn create(&self, _: &Settings, _: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        Err(PluginError::Unavailable("test".into()))
    }
}

struct TestFilter(&'static str, FilterTarget);
impl Plugin for TestFilter {
    fn id(&self) -> &'static str {
        self.0
    }
    fn name(&self) -> &'static str {
        "Gain"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::new(vec![Field::new("db", "Gain", FieldKind::Float { min: -30.0, max: 30.0, step: 0.1 }, 0.0)])
    }
}
impl Filter for TestFilter {
    fn target(&self) -> FilterTarget {
        self.1
    }
    fn create(&self, _: &Settings) -> Result<Box<dyn FilterInstance>, PluginError> {
        Err(PluginError::Unavailable("test".into()))
    }
}

struct TestTransition(&'static str);
impl Plugin for TestTransition {
    fn id(&self) -> &'static str {
        self.0
    }
    fn name(&self) -> &'static str {
        "Fade"
    }
    fn schema(&self) -> SettingsSchema {
        SettingsSchema::default()
    }
}
impl Transition for TestTransition {
    fn shader(&self) -> &'static str {
        ""
    }
}

fn engine() -> Engine {
    let mut r = Registry::new();
    r.register_source(Arc::new(TestSource("color", SourceCaps { video: true, audio: false }))).unwrap();
    r.register_source(Arc::new(TestSource("mic", SourceCaps { video: false, audio: true }))).unwrap();
    r.register_filter(Arc::new(TestFilter("gain", FilterTarget::Audio))).unwrap();
    r.register_filter(Arc::new(TestFilter("tint", FilterTarget::Video))).unwrap();
    r.register_transition(Arc::new(TestTransition("fade"))).unwrap();
    r.register_transition(Arc::new(TestTransition("cut"))).unwrap();
    Engine::new(SceneCollection::new("Test", "Scene"), Arc::new(r))
}

fn run(e: &mut Engine, id: &str, params: serde_json::Value) -> Result<Outcome, EngineError> {
    e.execute(Command::from_parts(id, &params).map_err(EngineError::Invalid)?)
}

fn add_color(e: &mut Engine, name: &str) -> (SourceId, ItemId) {
    let o = run(e, "source.add", json!({"kind": "color", "name": name})).unwrap();
    (serde_json::from_value(o.result["source"].clone()).unwrap(), serde_json::from_value(o.result["item"].clone()).unwrap())
}

fn names(e: &Engine) -> Vec<String> {
    e.collection().scenes.iter().map(|s| s.name.clone()).collect()
}

fn item_sources(e: &Engine) -> Vec<String> {
    let c = e.collection();
    c.scene(e.edit_scene()).unwrap().items.iter().map(|i| c.source(i.source).unwrap().name.clone()).collect()
}

#[test]
fn scene_add_rename_duplicate_move_remove() {
    let mut e = engine();
    run(&mut e, "scene.add", json!({})).unwrap();
    run(&mut e, "scene.add", json!({"name": "Break"})).unwrap();
    assert_eq!(names(&e), ["Scene", "Scene 2", "Break"]);
    assert_eq!(e.collection().scene(e.collection().program_scene).unwrap().name, "Break", "a new scene is shown");
    assert!(matches!(run(&mut e, "scene.add", json!({"name": "Break"})), Err(EngineError::NameInUse(_))));
    run(&mut e, "scene.rename", json!({"scene": "Scene 2", "name": "Game"})).unwrap();
    assert!(matches!(run(&mut e, "scene.rename", json!({"scene": "Game", "name": "  "})), Err(EngineError::BadName)));
    run(&mut e, "scene.duplicate", json!({"scene": "Game"})).unwrap();
    assert_eq!(names(&e), ["Scene", "Game", "Game 2", "Break"]);
    run(&mut e, "scene.move", json!({"scene": "Break", "to": "top"})).unwrap();
    run(&mut e, "scene.move", json!({"scene": "Scene", "to": "down"})).unwrap();
    assert_eq!(names(&e), ["Break", "Game", "Scene", "Game 2"]);
    run(&mut e, "scene.move", json!({"scene": "Game 2", "to": {"to": 1}})).unwrap();
    assert_eq!(names(&e), ["Break", "Game 2", "Game", "Scene"]);
    run(&mut e, "scene.remove", json!({"scene": "Break"})).unwrap();
    assert_eq!(e.collection().scene(e.collection().program_scene).unwrap().name, "Game 2", "the program moves to a neighbor");
    for n in ["Game 2", "Game"] {
        run(&mut e, "scene.remove", json!({"scene": n})).unwrap();
    }
    assert!(matches!(run(&mut e, "scene.remove", json!({"scene": "Scene"})), Err(EngineError::LastScene)));
    assert!(e.collection().problems().is_empty());
}

#[test]
fn scene_thumbnails_name_known_scenes_once_at_a_bounded_rate() {
    let mut e = engine();
    run(&mut e, "scene.add", json!({"name": "B"})).unwrap();
    let thumbnails = |e: &mut Engine, params| run(e, "scene.thumbnails", params).map(|o| (o.changed, o.runtime));
    let b = e.collection().scene_by_name("B").unwrap().id;
    let wanted = |scenes, fps| (false, Some(RuntimeAction::SceneThumbnails { scenes, fps }));
    // By id or name, in the order given, 2 a second when left out.
    assert_eq!(thumbnails(&mut e, json!({"scenes": ["B", 1]})).unwrap(), wanted(vec![b, SceneId(1)], 2.0));
    assert_eq!(thumbnails(&mut e, json!({"scenes": [1], "fps": 4})).unwrap(), wanted(vec![SceneId(1)], 4.0));
    assert_eq!(thumbnails(&mut e, json!({"scenes": [1], "fps": 0.1})).unwrap(), wanted(vec![SceneId(1)], 0.1));
    // An empty list stops drawing.
    assert_eq!(thumbnails(&mut e, json!({"scenes": []})).unwrap(), wanted(vec![], 2.0));
    // Bad lists and rates are errors, not clamped.
    assert!(matches!(thumbnails(&mut e, json!({"scenes": ["Nope"]})), Err(EngineError::NoScene(_))));
    assert!(matches!(thumbnails(&mut e, json!({"scenes": [99]})), Err(EngineError::NoScene(_))));
    let twice = thumbnails(&mut e, json!({"scenes": [1, "Scene"]})).unwrap_err();
    assert!(twice.to_string().contains("more than once"), "{twice}");
    // A rate too low to repeat within a `Duration` (1e-20) would draw once, so it is refused too.
    for fps in [json!(0), json!(-1), json!(0.09), json!(1e-20), json!(4.01), json!(60), json!(null), json!("2")] {
        assert!(thumbnails(&mut e, json!({"scenes": [1], "fps": fps})).is_err(), "fps {fps}");
    }
    assert!(thumbnails(&mut e, json!({})).is_err(), "the list is required");
    assert!(thumbnails(&mut e, json!({"scenes": null})).is_err());
    // At most 16 scenes.
    for i in 0..15 {
        run(&mut e, "scene.add", json!({"name": format!("S{i}")})).unwrap();
    }
    let all: Vec<SceneId> = e.collection().scenes.iter().map(|s| s.id).collect();
    assert_eq!(all.len(), 17);
    assert_eq!(thumbnails(&mut e, json!({"scenes": all[..16]})).unwrap(), wanted(all[..16].to_vec(), 2.0));
    let many = thumbnails(&mut e, json!({"scenes": all})).unwrap_err();
    assert!(many.to_string().contains("more than 16 scenes"), "{many}");
    // A built command with too many scenes is refused too.
    let built = e.execute(C::SceneThumbnails { scenes: all.iter().map(|&id| SceneRef::Id(id)).collect(), fps: None }).unwrap_err();
    assert!(built.to_string().contains("at most 16"), "{built}");
    // Nothing changes in the collection or its history.
    let (revision, undo) = (e.revision(), e.undo_label());
    thumbnails(&mut e, json!({"scenes": [1]})).unwrap();
    assert_eq!((e.revision(), e.undo_label()), (revision, undo));
}

#[test]
fn duplicated_scenes_share_their_sources() {
    let mut e = engine();
    let (src, _) = add_color(&mut e, "Bg");
    run(&mut e, "scene.duplicate", json!({"scene": "Scene", "name": "Copy"})).unwrap();
    let copy = e.collection().scene_by_name("Copy").unwrap();
    assert_eq!(copy.items[0].source, src);
    assert_ne!(copy.items[0].id, e.collection().scene_by_name("Scene").unwrap().items[0].id);
}

#[test]
fn select_switches_program_or_preview() {
    let mut e = engine();
    run(&mut e, "scene.add", json!({"name": "B"})).unwrap();
    run(&mut e, "scene.select", json!({"scene": "Scene"})).unwrap();
    assert_eq!(e.collection().program_scene, SceneId(1));
    run(&mut e, "scene.select_index", json!({"index": 2})).unwrap();
    assert_eq!(e.collection().scene(e.collection().program_scene).unwrap().name, "B");
    assert!(run(&mut e, "scene.select_index", json!({"index": 3})).is_err());
    assert!(run(&mut e, "scene.select_index", json!({"index": 0})).is_err());

    run(&mut e, "studio.toggle", json!({})).unwrap();
    assert!(e.studio_mode());
    run(&mut e, "scene.select", json!({"scene": "Scene"})).unwrap();
    assert_eq!(e.collection().scene(e.collection().program_scene).unwrap().name, "B", "studio mode: the program stays");
    assert_eq!(e.edit_scene(), SceneId(1));
    assert!(run(&mut e, "studio.transition", json!({"kind": "no_such_transition"})).is_err());
    assert!(run(&mut e, "studio.transition", json!({"ms": 999_999})).is_err());
    assert_eq!(e.collection().scene(e.collection().program_scene).unwrap().name, "B", "a refused switch leaves the program");
    let o = run(&mut e, "studio.transition", json!({})).unwrap();
    assert!(o.changed);
    assert_eq!(e.collection().program_scene, SceneId(1));
    run(&mut e, "studio.toggle", json!({})).unwrap();
    assert!(!e.studio_mode());
    assert!(run(&mut e, "studio.transition", json!({})).is_err());
}

#[test]
fn sources_add_rename_duplicate_reorder_remove() {
    let mut e = engine();
    add_color(&mut e, "A");
    add_color(&mut e, "B");
    let o = run(&mut e, "source.add", json!({"kind": "color"})).unwrap();
    assert!(o.changed);
    assert_eq!(item_sources(&e), ["A", "B", "Color"]);
    assert!(run(&mut e, "source.add", json!({"kind": "nope"})).is_err());
    assert!(matches!(run(&mut e, "source.add", json!({"kind": "color", "name": "A"})), Err(EngineError::NameInUse(_))));
    run(&mut e, "source.rename", json!({"source": "Color", "name": "C"})).unwrap();
    // The list shows the top first: C, B, A. Moving A up puts it above B.
    run(&mut e, "item.move", json!({"item": "A", "to": "up"})).unwrap();
    assert_eq!(item_sources(&e), ["B", "A", "C"]);
    run(&mut e, "item.move", json!({"item": "B", "to": "top"})).unwrap();
    assert_eq!(item_sources(&e), ["A", "C", "B"]);
    run(&mut e, "item.move", json!({"item": "B", "to": {"to": 2}})).unwrap();
    assert_eq!(item_sources(&e), ["B", "A", "C"]);
    run(&mut e, "item.duplicate", json!({"item": "A"})).unwrap();
    assert_eq!(item_sources(&e), ["B", "A", "A 2", "C"]);
    run(&mut e, "item.remove", json!({"item": "A 2"})).unwrap();
    assert!(e.collection().source_by_name("A 2").is_none(), "an unused source goes with its last item");
    run(&mut e, "source.add_existing", json!({"source": "A"})).unwrap();
    assert_eq!(item_sources(&e), ["B", "A", "C", "A"]);
    assert!(e.collection().problems().is_empty());
}

#[test]
fn settings_are_checked_and_completed_by_the_plugin_schema() {
    let mut e = engine();
    let (src, _) = add_color(&mut e, "A");
    assert_eq!(e.collection().source(src).unwrap().settings.i64("width"), Some(10), "defaults filled in");
    run(&mut e, "source.settings", json!({"source": "A", "settings": {"width": 60}})).unwrap();
    assert_eq!(e.collection().source(src).unwrap().settings.i64("width"), Some(60));
    for bad in [json!(1000), json!(0), json!("wide")] {
        let r = run(&mut e, "source.settings", json!({"source": "A", "settings": {"width": bad}}));
        assert!(matches!(&r, Err(EngineError::Invalid(m)) if m.contains("`width`")), "{bad}");
    }
    assert_eq!(e.collection().source(src).unwrap().settings.i64("width"), Some(60), "a refused change leaves the settings");
    assert!(run(&mut e, "source.add", json!({"kind": "color", "name": "B", "settings": {"width": 1000}})).is_err());
    assert!(e.collection().source_by_name("B").is_none());
}

#[test]
fn a_saved_value_out_of_range_does_not_block_other_changes() {
    let mut e = engine();
    let (src, _) = add_color(&mut e, "A");
    // An out-of-range value can only come from outside the commands, as from a file.
    e.collection.source_mut(src).unwrap().settings.set("width", 1000);
    run(&mut e, "source.settings", json!({"source": "A", "settings": {"width": 1000, "extra": true}})).unwrap();
    assert_eq!(e.collection().source(src).unwrap().settings.i64("width"), Some(100), "brought into range");
}

#[test]
fn transforms_out_of_range_are_refused() {
    let mut e = engine();
    let (_, item) = add_color(&mut e, "A");
    let before = e.collection().scene(SceneId(1)).unwrap().item(item).unwrap().transform;
    for bad in [json!({"position": {"x": 1e9, "y": 0.0}}), json!({"scale": {"x": 0.0, "y": 1.0}}), json!({"scale": {"x": 1.0, "y": 5000.0}})] {
        let r = run(&mut e, "item.transform", json!({"item": item, "transform": bad}));
        assert!(matches!(r, Err(EngineError::Invalid(_))), "{bad}: {r:?}");
        let r = run(&mut e, "source.add", json!({"kind": "color", "name": "B", "transform": bad}));
        assert!(matches!(r, Err(EngineError::Invalid(_))), "{bad}: {r:?}");
    }
    assert_eq!(e.collection().scene(SceneId(1)).unwrap().item(item).unwrap().transform, before, "nothing changed");
    assert!(e.collection().source_by_name("B").is_none(), "no source was added");
}

#[test]
fn transform_visibility_and_lock() {
    let mut e = engine();
    let (_, item) = add_color(&mut e, "A");
    let t = Transform { position: Vec2::new(10.0, 20.0), scale: Vec2::new(0.5, 0.5), rotation: 450.0, ..Transform::default() };
    run(&mut e, "item.transform", json!({"item": item, "transform": t})).unwrap();
    let got = e.collection().scene(SceneId(1)).unwrap().item(item).unwrap().transform;
    assert_eq!((got.position, got.rotation), (Vec2::new(10.0, 20.0), 90.0));
    run(&mut e, "item.visible", json!({"item": "A", "visible": false})).unwrap();
    run(&mut e, "item.lock", json!({"item": "A", "locked": true})).unwrap();
    assert!(matches!(run(&mut e, "item.transform", json!({"item": "A", "transform": Transform::default()})), Err(EngineError::Locked(_))));
    let it = e.collection().scene(SceneId(1)).unwrap().item(item).unwrap();
    assert!(!it.visible && it.locked);
}

#[test]
fn canvas_relayout_places_every_scene_in_one_undo_step() {
    let mut e = engine();
    let (fit_src, fit) = add_color(&mut e, "Fit");
    let fit_t = Transform::fit((640, 480), (1920, 1080));
    run(&mut e, "item.transform", json!({"item": fit, "transform": fit_t, "fit": "fit"})).unwrap();
    run(&mut e, "scene.add", json!({"name": "Other"})).unwrap();
    let (_, corner) = add_color(&mut e, "Corner");
    let corner_t = Transform { position: Vec2::new(960.0, 540.0), ..Transform::default() };
    run(&mut e, "item.transform", json!({"item": corner, "transform": corner_t})).unwrap();
    run(&mut e, "item.lock", json!({"item": corner, "locked": true})).unwrap();
    e.seal_history();
    let before = e.collection().clone();
    let size = |w: u32, h: u32| json!({"width": w, "height": h});
    // Bad parameters are refused and change nothing.
    for bad in [
        json!({"from": size(8, 1080), "to": size(1280, 720)}),
        json!({"from": size(1920, 1080), "to": size(1280, 9000)}),
        json!({"from": size(1920, 1080), "to": size(1280, 720), "sizes": [{"source": 999, "width": 640, "height": 480}]}),
        json!({"from": size(1920, 1080), "to": size(1280, 720), "sizes": [{"source": fit_src, "width": 0, "height": 480}]}),
        json!({"from": size(1920, 1080), "to": size(1280, 720), "sizes": [{"source": fit_src, "width": 640, "height": 480}, {"source": fit_src, "width": 640, "height": 480}]}),
        json!({"from": size(1920, 1080), "to": "1280x720"}),
        json!({"from": size(1920, 1080), "to": size(1280, 720), "items": [999]}),
        json!({"from": size(1920, 1080), "to": size(1280, 720), "items": [fit, fit]}),
    ] {
        assert!(run(&mut e, "canvas.relayout", bad.clone()).is_err(), "{bad}");
    }
    assert_eq!(e.collection(), &before);
    let sizes = json!([{"source": fit_src, "width": 640, "height": 480}]);
    run(&mut e, "canvas.relayout", json!({"from": size(1920, 1080), "to": size(1080, 1920), "sizes": sizes})).unwrap();
    let c = e.collection();
    let t = |item: ItemId| c.scenes.iter().find_map(|s| s.item(item)).unwrap().transform;
    assert_eq!(t(fit), Transform::fit((640, 480), (1080, 1920)), "a fit item is fit again");
    // Without a size, the top-left corner keeps its place in proportion; locked items move too.
    let p = t(corner).position;
    assert!((p.x - 540.0).abs() < 0.01 && (p.y - 960.0).abs() < 0.01, "{p:?}");
    run(&mut e, "edit.undo", json!({})).unwrap();
    assert_eq!(e.collection().scenes, before.scenes, "one undo step takes the whole layout back");
    // The same size again changes nothing and records no step.
    let same = run(&mut e, "canvas.relayout", json!({"from": size(1920, 1080), "to": size(1920, 1080), "sizes": sizes})).unwrap();
    assert!(!same.changed);
    assert_eq!(e.redo_label(), Some("Lay Out for Canvas Size"), "a recorded step would have dropped the redo");
    // With `items`, only those items are placed.
    run(&mut e, "canvas.relayout", json!({"from": size(1920, 1080), "to": size(1080, 1920), "sizes": sizes, "items": [corner]})).unwrap();
    let c = e.collection();
    let t = |item: ItemId| c.scenes.iter().find_map(|s| s.item(item)).unwrap().transform;
    let was = |item: ItemId| before.scenes.iter().find_map(|s| s.item(item)).unwrap().transform;
    assert_eq!(t(fit), was(fit), "an item not listed stays");
    assert_ne!(t(corner), was(corner));
}

#[test]
fn a_fit_mode_stays_until_the_transform_changes_another_way() {
    let mut e = engine();
    let (_, item) = add_color(&mut e, "Back");
    let fit_of = |e: &Engine| e.collection().scenes[0].item(item).unwrap().fit;
    let t = Transform::fit((640, 480), (1920, 1080));
    run(&mut e, "item.transform", json!({"item": item, "transform": t, "fit": "fill"})).unwrap();
    assert_eq!(fit_of(&e), Some(FitMode::Fill));
    assert!(run(&mut e, "item.transform", json!({"item": item, "transform": t, "fit": "cover"})).is_err());
    assert_eq!(fit_of(&e), Some(FitMode::Fill), "a bad mode changes nothing");
    // A duplicate keeps the mode; a move without one clears it.
    let dup = run(&mut e, "item.duplicate", json!({"item": item})).unwrap().result["item"].as_u64().unwrap();
    assert_eq!(e.collection().scenes[0].item(ItemId(dup)).unwrap().fit, Some(FitMode::Fill));
    let moved = Transform { position: Vec2::new(10.0, 0.0), ..t };
    run(&mut e, "item.transform", json!({"item": item, "transform": moved})).unwrap();
    assert_eq!(fit_of(&e), None);
}

#[test]
fn volume_and_mute() {
    let mut e = engine();
    let o = run(&mut e, "source.add", json!({"kind": "mic", "name": "Mic"})).unwrap();
    let src: SourceId = serde_json::from_value(o.result["source"].clone()).unwrap();
    run(&mut e, "source.volume", json!({"source": "Mic", "gain": 0.25})).unwrap();
    for bad in [11.0, -0.1] {
        assert!(matches!(run(&mut e, "source.volume", json!({"source": "Mic", "gain": bad})), Err(EngineError::Invalid(_))), "{bad}");
    }
    assert_eq!(e.collection().source(src).unwrap().audio.gain, 0.25);
    run(&mut e, "source.mute", json!({"source": src, "muted": true})).unwrap();
    assert!(e.collection().source(src).unwrap().audio.muted);
    assert!(run(&mut e, "source.volume", json!({"source": "Mic", "gain": "loud"})).is_err());
}

#[test]
fn volume_boosts_up_to_the_top() {
    use studiocraft_model::collection::MAX_GAIN;
    let mut e = engine();
    let o = run(&mut e, "source.add", json!({"kind": "mic", "name": "Mic"})).unwrap();
    let src: SourceId = serde_json::from_value(o.result["source"].clone()).unwrap();
    let gain = |e: &Engine| e.collection().source(src).unwrap().audio.gain;
    run(&mut e, "source.volume", json!({"source": "Mic", "gain": 4.0})).unwrap();
    assert_eq!(gain(&e), 4.0);
    e.seal_history();
    run(&mut e, "source.volume", json!({"source": "Mic", "gain": MAX_GAIN})).unwrap();
    assert_eq!(gain(&e), MAX_GAIN, "the top is accepted");
    e.seal_history();
    for bad in [MAX_GAIN + 0.01, 1000.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -0.01] {
        let err = e.execute(C::SourceVolume { source: SourceRef::Id(src), gain: bad }).unwrap_err();
        assert!(matches!(&err, EngineError::Invalid(m) if m == "the volume must be a number from 0 to 10 (+20 dB)"), "{bad}: {err:?}");
        assert_eq!(gain(&e), MAX_GAIN, "{bad} changes nothing");
    }
    // Setting the volume it already has is no change and no undo step: one undo goes back to 4.
    let rev = e.revision();
    assert!(!run(&mut e, "source.volume", json!({"source": "Mic", "gain": MAX_GAIN})).unwrap().changed);
    assert_eq!(e.revision(), rev);
    run(&mut e, "edit.undo", json!({})).unwrap();
    assert_eq!(gain(&e), 4.0);
}

#[test]
fn sync_offset() {
    let mut e = engine();
    let o = run(&mut e, "source.add", json!({"kind": "mic", "name": "Mic"})).unwrap();
    let src: SourceId = serde_json::from_value(o.result["source"].clone()).unwrap();
    run(&mut e, "source.sync_offset", json!({"source": "Mic", "ms": 250})).unwrap();
    assert_eq!(e.collection().source(src).unwrap().audio.sync_offset_ms, 250);
    e.seal_history();
    run(&mut e, "source.sync_offset", json!({"source": src, "ms": 5000})).unwrap();
    for bad in [json!(5001), json!(-1), json!(12.5), json!("late"), json!(u64::MAX)] {
        assert!(run(&mut e, "source.sync_offset", json!({"source": "Mic", "ms": bad})).is_err(), "{bad}");
    }
    assert_eq!(e.collection().source(src).unwrap().audio.sync_offset_ms, 5000, "a refused offset changes nothing");
    assert!(run(&mut e, "source.sync_offset", json!({"source": "Mic"})).is_err(), "the offset is required");
    assert!(!run(&mut e, "source.sync_offset", json!({"source": "Mic", "ms": 5000})).unwrap().changed, "the same offset is no change");
    run(&mut e, "edit.undo", json!({})).unwrap();
    assert_eq!(e.collection().source(src).unwrap().audio.sync_offset_ms, 250);
    add_color(&mut e, "Video");
    assert!(
        matches!(run(&mut e, "source.sync_offset", json!({"source": "Video", "ms": 10})), Err(EngineError::Invalid(m)) if m.contains("no audio"))
    );
    assert!(matches!(run(&mut e, "source.sync_offset", json!({"source": "ghost", "ms": 10})), Err(EngineError::NoSource(_))));
}

#[test]
fn monitoring_mode() {
    use studiocraft_model::MonitorMode;
    let mut e = engine();
    let o = run(&mut e, "source.add", json!({"kind": "mic", "name": "Mic"})).unwrap();
    let src: SourceId = serde_json::from_value(o.result["source"].clone()).unwrap();
    let mode = |e: &Engine| e.collection().source(src).unwrap().audio.monitor;
    assert_eq!(mode(&e), MonitorMode::Off);
    run(&mut e, "source.monitor", json!({"source": "Mic", "mode": "monitor_only"})).unwrap();
    assert_eq!(mode(&e), MonitorMode::MonitorOnly);
    run(&mut e, "source.monitor", json!({"source": src, "mode": "monitor_and_output"})).unwrap();
    assert_eq!(mode(&e), MonitorMode::MonitorAndOutput);
    for bad in [json!("loud"), json!(""), json!("Monitor_Only"), json!(1), json!(null)] {
        assert!(run(&mut e, "source.monitor", json!({"source": "Mic", "mode": bad})).is_err(), "{bad}");
    }
    assert!(run(&mut e, "source.monitor", json!({"source": "Mic"})).is_err(), "the mode is required");
    assert_eq!(mode(&e), MonitorMode::MonitorAndOutput, "a refused mode changes nothing");
    assert!(!run(&mut e, "source.monitor", json!({"source": "Mic", "mode": "monitor_and_output"})).unwrap().changed, "the same mode is no change");
    run(&mut e, "edit.undo", json!({})).unwrap();
    assert_eq!(mode(&e), MonitorMode::MonitorOnly, "each change is its own undo step");
    add_color(&mut e, "Video");
    assert!(
        matches!(run(&mut e, "source.monitor", json!({"source": "Video", "mode": "monitor_only"})), Err(EngineError::Invalid(m)) if m.contains("no audio"))
    );
    assert!(matches!(run(&mut e, "source.monitor", json!({"source": "ghost", "mode": "off"})), Err(EngineError::NoSource(_))));
}

#[test]
fn filters_match_what_the_source_produces() {
    let mut e = engine();
    add_color(&mut e, "Video");
    run(&mut e, "source.add", json!({"kind": "mic", "name": "Mic"})).unwrap();
    assert!(matches!(run(&mut e, "filter.add", json!({"source": "Video", "kind": "gain"})), Err(EngineError::FilterMismatch(..))));
    let o = run(&mut e, "filter.add", json!({"source": "Mic", "kind": "gain"})).unwrap();
    let f = o.result["filter"].as_u64().unwrap();
    assert!(matches!(run(&mut e, "filter.settings", json!({"source": "Mic", "filter": f, "settings": {"db": 99}})), Err(EngineError::Invalid(_))));
    run(&mut e, "filter.settings", json!({"source": "Mic", "filter": f, "settings": {"db": 12.5}})).unwrap();
    run(&mut e, "filter.enable", json!({"source": "Mic", "filter": f, "enabled": false})).unwrap();
    let fc = &e.collection().source_by_name("Mic").unwrap().filters[0];
    assert_eq!((fc.settings.f64("db"), fc.enabled), (Some(12.5), false));
    run(&mut e, "filter.remove", json!({"source": "Mic", "filter": f})).unwrap();
    assert!(matches!(run(&mut e, "filter.remove", json!({"source": "Mic", "filter": f})), Err(EngineError::NoFilter(_))));
    run(&mut e, "filter.add", json!({"source": "Video", "kind": "tint", "name": "Warm"})).unwrap();
}

#[test]
fn a_filter_meter_reading_is_handed_to_the_app_for_a_filter_of_that_source() {
    let mut e = engine();
    add_color(&mut e, "Video");
    let mic = run(&mut e, "source.add", json!({"kind": "mic", "name": "Mic"})).unwrap().result["source"].as_u64().unwrap();
    let f = run(&mut e, "filter.add", json!({"source": "Mic", "kind": "gain"})).unwrap().result["filter"].as_u64().unwrap();
    let expected = RuntimeAction::FilterMeter { source: SourceId(mic), filter: FilterId(f) };
    for (source, enabled) in [(json!("Mic"), true), (json!(mic), true), (json!("Mic"), false)] {
        run(&mut e, "filter.enable", json!({"source": "Mic", "filter": f, "enabled": enabled})).unwrap();
        let (rev, history) = (e.revision(), e.undo_label());
        let o = run(&mut e, "filter.meter", json!({"source": source, "filter": f})).unwrap();
        assert_eq!(o.runtime, Some(expected.clone()), "{source}, enabled {enabled}");
        assert!(!o.changed);
        assert_eq!((e.revision(), e.undo_label()), (rev, history), "a reading is not a change or an undo step");
    }
    assert!(matches!(run(&mut e, "filter.meter", json!({"source": "Video", "filter": f})), Err(EngineError::NoFilter(n)) if n == f));
    assert!(matches!(run(&mut e, "filter.meter", json!({"source": "Mic", "filter": f + 1000})), Err(EngineError::NoFilter(_))));
    assert!(matches!(run(&mut e, "filter.meter", json!({"source": "ghost", "filter": f})), Err(EngineError::NoSource(_))));
    for bad in [
        json!({"source": "Mic"}),
        json!({"filter": f}),
        json!({"source": "Mic", "filter": -1}),
        json!({"source": "Mic", "filter": "1"}),
        json!({"source": "Mic", "filter": null}),
        json!({"source": "Mic", "filter": f, "flter": f}),
    ] {
        assert!(run(&mut e, "filter.meter", bad.clone()).is_err(), "{bad}");
    }
}

#[test]
fn transitions() {
    let mut e = engine();
    run(&mut e, "transition.set", json!({"kind": "cut"})).unwrap();
    assert!(run(&mut e, "transition.set", json!({"kind": "wipe"})).is_err());
    assert!(matches!(run(&mut e, "transition.duration", json!({"ms": 1_000_000})), Err(EngineError::Invalid(_))));
    run(&mut e, "transition.duration", json!({"ms": MAX_TRANSITION_MS})).unwrap();
    assert_eq!(e.collection().transition.kind, "cut");
    assert_eq!(e.collection().transition.duration_ms, MAX_TRANSITION_MS);
    run(&mut e, "transition.settings", json!({"settings": {"anything": 1}})).unwrap();
    run(&mut e, "edit.undo", json!({})).unwrap();
    assert_eq!(e.collection().transition.kind, "cut", "settings undo on their own");
}

#[test]
fn undo_and_redo_restore_edits_but_not_the_program() {
    let mut e = engine();
    add_color(&mut e, "A");
    run(&mut e, "scene.add", json!({"name": "B"})).unwrap();
    assert_eq!(e.undo_label(), Some("Add Scene"));
    run(&mut e, "edit.undo", json!({})).unwrap();
    assert_eq!(names(&e), ["Scene"]);
    assert_eq!(e.collection().program_scene, SceneId(1), "the program falls back when its scene is undone");
    run(&mut e, "edit.redo", json!({})).unwrap();
    assert_eq!(names(&e), ["Scene", "B"]);
    run(&mut e, "scene.select", json!({"scene": "Scene"})).unwrap();
    run(&mut e, "edit.undo", json!({})).unwrap();
    run(&mut e, "edit.undo", json!({})).unwrap();
    assert!(e.collection().sources.is_empty());
    assert!(!run(&mut e, "edit.undo", json!({})).unwrap().changed, "nothing left to undo");
}

#[test]
fn a_drag_is_one_undo_step() {
    let mut e = engine();
    let (_, item) = add_color(&mut e, "A");
    for x in 0..20 {
        let t = Transform { position: Vec2::new(x as f32, 0.0), ..Transform::default() };
        run(&mut e, "item.transform", json!({"item": item, "transform": t})).unwrap();
    }
    e.seal_history();
    run(&mut e, "edit.undo", json!({})).unwrap();
    let it = e.collection().scene(SceneId(1)).unwrap().item(item).unwrap();
    assert_eq!(it.transform.position, Vec2::ZERO);
}

#[test]
fn runtime_commands_are_handed_to_the_app() {
    let mut e = engine();
    for (id, action) in [
        ("record.start", RuntimeAction::StartRecording),
        ("record.stop", RuntimeAction::StopRecording),
        ("record.toggle", RuntimeAction::ToggleRecording),
        ("recordings.list", RuntimeAction::RecordingsList),
        ("stream.start", RuntimeAction::StartStreaming),
        ("stream.stop", RuntimeAction::StopStreaming),
        ("stream.toggle", RuntimeAction::ToggleStreaming),
        ("golive.check", RuntimeAction::GoLiveCheck),
        ("replay.start", RuntimeAction::StartReplay),
        ("replay.stop", RuntimeAction::StopReplay),
        ("replay.toggle", RuntimeAction::ToggleReplay),
        ("replay.save", RuntimeAction::SaveReplay),
        ("audio.loudness", RuntimeAction::AudioLoudness),
        ("audio_tracks.status", RuntimeAction::AudioTracksStatus),
        ("audio_tracks.open_last", RuntimeAction::AudioTracksOpenLast),
        ("hardware_encoders.retest", RuntimeAction::RetestHardwareEncoders),
        ("app.settings", RuntimeAction::OpenSettings),
        ("app.exit", RuntimeAction::Exit),
    ] {
        let o = run(&mut e, id, json!({})).unwrap();
        assert_eq!(o.runtime, Some(action), "{id}");
        assert!(!o.changed);
    }
    assert!(run(&mut e, "golive.check", json!({"items": ["encoder"]})).is_err(), "the check takes no parameters");
    let o = run(&mut e, "audio_tracks.repair", json!({"folder": "D:/Videos"})).unwrap();
    assert_eq!(o.runtime, Some(RuntimeAction::AudioTracksRepair { folder: "D:/Videos".into() }));
    assert!(!o.changed);
    for bad in [json!({}), json!({"folder": ""}), json!({"folder": "  "}), json!({"folder": 3})] {
        assert!(run(&mut e, "audio_tracks.repair", bad.clone()).is_err(), "{bad}");
    }
    let o = run(&mut e, "audio_tracks.marker", json!({})).unwrap();
    assert_eq!(o.runtime, Some(RuntimeAction::AudioTracksMarker { name: None }));
    let longest = "é".repeat(crate::engine::MAX_MARKER_NAME_CHARS);
    let o = run(&mut e, "audio_tracks.marker", json!({"name": longest})).unwrap();
    assert_eq!(o.runtime, Some(RuntimeAction::AudioTracksMarker { name: Some(longest) }));
    assert!(!o.changed);
    let too_long = "x".repeat(crate::engine::MAX_MARKER_NAME_CHARS + 1);
    for bad in
        [json!({"name": ""}), json!({"name": "   "}), json!({"name": too_long}), json!({"name": "a\nb"}), json!({"name": 3}), json!({"name": null})]
    {
        assert!(run(&mut e, "audio_tracks.marker", bad.clone()).is_err(), "{bad}");
    }
}

#[test]
fn a_recording_marker_is_handed_to_the_app_with_a_checked_name() {
    let mut e = engine();
    let o = run(&mut e, "record.marker", json!({})).unwrap();
    assert_eq!(o.runtime, Some(RuntimeAction::RecordingMarker { name: None }));
    let longest = "é".repeat(crate::engine::MAX_MARKER_NAME_CHARS);
    let o = run(&mut e, "record.marker", json!({"name": longest})).unwrap();
    assert_eq!(o.runtime, Some(RuntimeAction::RecordingMarker { name: Some(longest) }));
    assert!(!o.changed);
    let too_long = "x".repeat(crate::engine::MAX_MARKER_NAME_CHARS + 1);
    for bad in
        [json!({"name": ""}), json!({"name": "   "}), json!({"name": too_long}), json!({"name": "a\tb"}), json!({"name": 3}), json!({"name": null})]
    {
        assert!(run(&mut e, "record.marker", bad.clone()).is_err(), "{bad}");
    }
}

#[test]
fn a_session_marker_is_handed_to_the_app_with_a_checked_name() {
    let mut e = engine();
    let o = run(&mut e, "marker.add", json!({})).unwrap();
    assert_eq!(o.runtime, Some(RuntimeAction::MarkerAdd { name: None }));
    let longest = "é".repeat(crate::engine::MAX_MARKER_NAME_CHARS);
    let o = run(&mut e, "marker.add", json!({"name": longest})).unwrap();
    assert_eq!(o.runtime, Some(RuntimeAction::MarkerAdd { name: Some(longest) }));
    assert!(!o.changed);
    let too_long = "x".repeat(crate::engine::MAX_MARKER_NAME_CHARS + 1);
    for bad in
        [json!({"name": ""}), json!({"name": "   "}), json!({"name": too_long}), json!({"name": "a\rb"}), json!({"name": 3}), json!({"name": null})]
    {
        assert!(run(&mut e, "marker.add", bad.clone()).is_err(), "{bad}");
    }
    let o = run(&mut e, "marker.list", json!({})).unwrap();
    assert_eq!(o.runtime, Some(RuntimeAction::MarkerList));
    assert!(!o.changed);
    assert!(run(&mut e, "marker.list", json!({"index": 1})).is_err(), "marker.list takes no params");
}

#[test]
fn a_marker_rename_is_handed_to_the_app_with_a_checked_index_and_name() {
    let mut e = engine();
    let longest = "é".repeat(crate::engine::MAX_MARKER_NAME_CHARS);
    let o = run(&mut e, "marker.rename", json!({"index": 3, "name": longest})).unwrap();
    assert_eq!(o.runtime, Some(RuntimeAction::MarkerRename { index: 3, name: longest }));
    assert!(!o.changed);
    let too_long = "x".repeat(crate::engine::MAX_MARKER_NAME_CHARS + 1);
    for bad in [
        json!({"index": 1}),
        json!({"name": "Goal"}),
        json!({"index": 0, "name": "Goal"}),
        json!({"index": -1, "name": "Goal"}),
        json!({"index": 1.5, "name": "Goal"}),
        json!({"index": "1", "name": "Goal"}),
        json!({"index": 1, "name": ""}),
        json!({"index": 1, "name": "   "}),
        json!({"index": 1, "name": too_long}),
        json!({"index": 1, "name": "a\nb"}),
        json!({"index": 1, "name": null}),
        json!({"index": 1, "name": "Goal", "extra": true}),
    ] {
        assert!(run(&mut e, "marker.rename", bad.clone()).is_err(), "{bad}");
    }
}

#[test]
fn pause_commands_are_handed_to_the_app_with_checked_values() {
    let mut e = engine();
    let rev = e.revision();
    for (id, params, action) in [
        ("pause.start", json!({}), RuntimeAction::PauseStart { media: None, countdown_s: None, at_zero: None }),
        (
            "pause.start",
            json!({"media": "", "countdown_s": 0, "at_zero": "keep"}),
            RuntimeAction::PauseStart { media: Some(String::new()), countdown_s: Some(0), at_zero: Some(PauseAtZero::Keep) },
        ),
        (
            "pause.start",
            json!({"media": "C:/break.png", "countdown_s": 43_200, "at_zero": "resume"}),
            RuntimeAction::PauseStart { media: Some("C:/break.png".into()), countdown_s: Some(43_200), at_zero: Some(PauseAtZero::Resume) },
        ),
        ("pause.stop", json!({}), RuntimeAction::PauseStop),
        ("pause.toggle", json!({}), RuntimeAction::PauseToggle),
        ("pause.extend", json!({"seconds": 1}), RuntimeAction::PauseExtend { seconds: 1 }),
        ("pause.extend", json!({"seconds": 43_200}), RuntimeAction::PauseExtend { seconds: 43_200 }),
        ("pause.status", json!({}), RuntimeAction::PauseStatus),
    ] {
        let o = run(&mut e, id, params.clone()).unwrap_or_else(|err| panic!("{id} {params}: {err}"));
        assert_eq!(o.runtime, Some(action), "{id} {params}");
        assert!(!o.changed, "{id} changes no collection");
    }
    assert_eq!(e.revision(), rev, "a pause is not a change to the collection");
    assert!(e.undo_label().is_none(), "a pause is not an undo step");
    for (id, bad) in [
        ("pause.start", json!({"countdown_s": 43_201})),
        ("pause.start", json!({"countdown_s": -1})),
        ("pause.start", json!({"countdown_s": "300"})),
        ("pause.start", json!({"countdown_s": null})),
        ("pause.start", json!({"at_zero": "restart"})),
        ("pause.start", json!({"at_zero": "Keep"})),
        ("pause.start", json!({"media": null})),
        ("pause.start", json!({"media": 5})),
        ("pause.start", json!({"media": "a\u{0}b.png"})),
        ("pause.start", json!({"meda": "x.png"})),
        ("pause.extend", json!({"seconds": 0})),
        ("pause.extend", json!({"seconds": 43_201})),
        ("pause.extend", json!({})),
    ] {
        assert!(run(&mut e, id, bad.clone()).is_err(), "{id} {bad}");
    }
}

#[test]
fn session_events_are_read_by_the_app_from_a_checked_number() {
    let mut e = engine();
    let rev = e.revision();
    for (params, since) in [(json!({}), None), (json!({"since": 0}), Some(0)), (json!({"since": u64::MAX}), Some(u64::MAX))] {
        let o = run(&mut e, "session.events", params.clone()).unwrap_or_else(|err| panic!("{params}: {err}"));
        assert_eq!(o.runtime, Some(RuntimeAction::SessionEvents { since }), "{params}");
        assert!(!o.changed);
    }
    assert_eq!(e.revision(), rev, "reading the events is not a change to the collection");
    assert!(e.undo_label().is_none(), "reading the events is not an undo step");
    for bad in [json!({"since": -1}), json!({"since": 1.5}), json!({"since": "3"}), json!({"since": null}), json!({"snce": 3})] {
        assert!(run(&mut e, "session.events", bad.clone()).is_err(), "{bad}");
    }
}

#[test]
fn caption_commands_are_handed_to_the_app() {
    let mut e = engine();
    for (id, params, action) in [
        ("captions.start", json!({}), RuntimeAction::CaptionsStart { model: None, language: None }),
        (
            "captions.start",
            json!({"model": "whisper-tiny", "language": "fr"}),
            RuntimeAction::CaptionsStart { model: Some("whisper-tiny".into()), language: Some("fr".into()) },
        ),
        ("captions.stop", json!({}), RuntimeAction::CaptionsStop),
        ("captions.toggle", json!({}), RuntimeAction::CaptionsToggle),
        ("captions.status", json!({}), RuntimeAction::CaptionsStatus),
        ("captions.download", json!({}), RuntimeAction::CaptionsDownload { model: None }),
        ("captions.download", json!({"model": "whisper-base"}), RuntimeAction::CaptionsDownload { model: Some("whisper-base".into()) }),
        ("captions.download_cancel", json!({}), RuntimeAction::CaptionsDownloadCancel),
    ] {
        let o = run(&mut e, id, params).unwrap();
        assert_eq!(o.runtime, Some(action), "{id}");
        assert!(!o.changed);
    }
    for (id, bad) in [
        ("captions.start", json!({"model": ""})),
        ("captions.start", json!({"language": " "})),
        ("captions.start", json!({"model": 3})),
        ("captions.start", json!({"modle": "whisper-tiny"})),
        ("captions.download", json!({"model": ""})),
        ("captions.download", json!({"model": 3})),
    ] {
        assert!(run(&mut e, id, bad.clone()).is_err(), "{id} {bad}");
    }
}

#[test]
fn a_source_is_set_to_record_its_own_track_or_not() {
    let mut e = engine();
    run(&mut e, "source.add", json!({"kind": "mic", "name": "Mic"})).unwrap();
    let flag = |e: &Engine| e.collection().source_by_name("Mic").map(|s| (s.audio.record_track, s.audio.records_track()));
    assert_eq!(flag(&e), Some((None, true)), "a source heard in the program records by default");
    let rev = e.revision();
    assert!(!run(&mut e, "source.set_record_track", json!({"source": "Mic", "on": true})).unwrap().changed, "on, as it already records");
    assert_eq!((flag(&e), e.revision()), (Some((None, true)), rev), "it keeps following its monitoring");
    run(&mut e, "source.monitor", json!({"source": "Mic", "mode": "monitor_only"})).unwrap();
    assert_eq!(flag(&e), Some((None, false)), "a monitor-only source does not until it is set");
    run(&mut e, "source.set_record_track", json!({"source": "Mic", "on": true})).unwrap();
    assert_eq!(flag(&e), Some((Some(true), true)));
    assert_eq!(e.undo_label(), Some("Record as Its Own Track"));
    let rev = e.revision();
    assert!(!run(&mut e, "source.set_record_track", json!({"source": "Mic", "on": true})).unwrap().changed, "the same value is no change");
    assert_eq!(e.revision(), rev);
    assert_eq!(e.undo_label(), Some("Record as Its Own Track"));
    run(&mut e, "edit.undo", json!({})).unwrap();
    assert_eq!(flag(&e), Some((None, false)), "one undo step");
    for bad in [json!({"source": "Mic"}), json!({"source": "Mic", "on": "yes"}), json!({"source": "Mic", "on": null})] {
        assert!(run(&mut e, "source.set_record_track", bad.clone()).is_err(), "{bad}");
    }
    add_color(&mut e, "Video");
    let silent = run(&mut e, "source.set_record_track", json!({"source": "Video", "on": true}));
    assert!(matches!(silent, Err(EngineError::Invalid(m)) if m.contains("no audio")));
    assert!(matches!(run(&mut e, "source.set_record_track", json!({"source": "ghost", "on": true})), Err(EngineError::NoSource(_))));
}

#[test]
fn link_and_info_queries_change_nothing() {
    let mut e = engine();
    run(&mut e, "scene.add", json!({"name": "Second"})).unwrap();
    e.mark_saved();
    let (rev, undo) = (e.revision(), e.undo_label());
    let all = run(&mut e, "app.links", json!({})).unwrap();
    assert!(!all.changed && all.runtime.is_none());
    for k in ["discord", "website", "github", "issues"] {
        assert!(all.result[k].as_str().is_some_and(|u| u.starts_with("https://")), "{k}");
    }
    assert_eq!(all.result.as_object().map(|o| o.len()), Some(4));
    let one = run(&mut e, "app.links", json!({"link": "github"})).unwrap();
    assert_eq!(one.result, json!({"url": crate::links::GITHUB}));
    assert!(run(&mut e, "app.links", json!({"link": "nope"})).is_err());
    assert!(run(&mut e, "app.links", json!({"link": 5})).is_err());
    let info = run(&mut e, "app.info", json!({})).unwrap();
    assert_eq!(info.result["name"], "StudioCraft");
    assert_eq!(info.result["license"], "MIT OR Apache-2.0");
    assert!(!info.changed);
    let plugins = run(&mut e, "plugin.list", json!({})).unwrap();
    assert!(!plugins.changed && plugins.runtime.is_none());
    assert_eq!(plugins.result["sources"][0]["id"], "color");
    assert!(plugins.result["sources"][0]["schema"]["fields"].is_array(), "{}", plugins.result);
    assert_eq!(plugins.result["transitions"][1]["id"], "cut");
    assert_eq!(plugins.result["filters"][0]["target"], "audio");
    let schema = run(&mut e, "source.schema", json!({"kind": "color"})).unwrap();
    assert!(!schema.changed && schema.runtime.is_none());
    assert_eq!(schema.result["schema"]["fields"][0]["key"], "width", "{}", schema.result);
    assert!(run(&mut e, "source.schema", json!({"kind": "nope"})).is_err());
    assert!(run(&mut e, "source.schema", json!({})).is_err(), "the kind is required");
    assert_eq!(e.revision(), rev);
    assert!(!e.is_dirty());
    assert_eq!(e.undo_label(), undo);
}

#[test]
fn application_audio_queries_list_apps_and_captures_without_changing_anything() {
    use craft_app_audio::fake::{FakeBackend, app};
    let fake = FakeBackend::new();
    let player = app("player.exe", 40, true);
    fake.set_peak(&player.key, 0.5);
    fake.set_apps(vec![player, app("browser.exe", 41, false)]);
    let mut e = engine();
    let discovery = craft_app_audio::Discovery::new(Arc::new(fake.clone()));
    e.set_app_audio(discovery.clone());
    let (rev, undo) = (e.revision(), e.undo_label());
    let wait = std::time::Duration::from_secs(10);
    // The first scan sticks in the system: the command answers at once, without applications.
    fake.hold_next_scan();
    let first = run(&mut e, "app_audio.list", json!({})).unwrap();
    assert!(!first.changed && first.runtime.is_none());
    assert_eq!(first.result["supported"], true);
    assert_eq!(first.result["listing"], true, "{}", first.result);
    assert_eq!(first.result["apps"], json!([]));
    assert!(fake.wait_scan_held(wait));
    fake.release_scan();
    assert!(discovery.fresh_list(wait).is_some());
    let list = run(&mut e, "app_audio.list", json!({})).unwrap();
    assert_eq!(list.result["listing"], false);
    assert_eq!(list.result["error"], serde_json::Value::Null);
    assert_eq!(list.result["apps"][0]["name"], "browser", "applications are listed by name: {}", list.result);
    assert_eq!(list.result["apps"][0]["target_pid"], 41);
    assert_eq!(list.result["apps"][1]["app"], r"player.exe|c:\apps\player.exe", "{}", list.result);
    assert_eq!(list.result["apps"][1]["playing"], true);
    assert_eq!(list.result["apps"][1]["within"], serde_json::Value::Null);
    // Asking again right after a meter reading shows the level.
    let passes = discovery.peak_passes();
    assert!(discovery.wait_peak_passes(passes + 1, wait));
    let again = run(&mut e, "app_audio.list", json!({})).unwrap();
    assert_eq!(again.result["apps"][1]["peak"], 0.5, "{}", again.result);
    // A scan sticks in the system: the answer keeps the last list and says the list is stuck,
    // while a new discovery thread starts.
    discovery.set_stall_limits(craft_app_audio::ACTIVATE_MS, craft_app_audio::WEDGE_MS, 0);
    fake.hold_next_scan();
    discovery.refresh();
    assert!(fake.wait_scan_stalled(&discovery, 2, wait), "the stuck scan shows as a stall");
    let stuck = run(&mut e, "app_audio.list", json!({})).unwrap();
    assert_eq!(stuck.result["error"], craft_app_audio::LIST_STALLED, "{}", stuck.result);
    assert_eq!(stuck.result["apps"].as_array().map(Vec::len), Some(2), "the last list is kept: {}", stuck.result);
    fake.release_scan();
    discovery.set_stall_limits(craft_app_audio::ACTIVATE_MS, craft_app_audio::WEDGE_MS, craft_app_audio::DISCOVERY_WEDGE_MS);
    let status = run(&mut e, "app_audio.status", json!({})).unwrap();
    assert_eq!(status.result["support"]["supported"], true);
    assert_eq!(status.result["discovery_stalled"], false);
    assert_eq!(status.result["captures"], 0);
    assert_eq!(status.result["max_captures"], 32);
    assert_eq!(status.result["discontinuities"], 0);
    // A capture whose system stream loses audio once: the status counts the gap.
    fake.set_behavior(41, craft_app_audio::fake::Behavior::Gap { frames: 480, after: 1, lost: 480 });
    let browser = craft_app_audio::AppKey::decode(r"browser.exe|c:\apps\browser.exe").unwrap();
    // The packet after the gap signals the test: the capture counted the gap before delivering it.
    let (after_gap, gap_counted) = std::sync::mpsc::sync_channel(1);
    let mut gap = false;
    let on_data = Box::new(move |_: &[f32], info: craft_app_audio::PacketInfo| {
        if gap {
            let _ = after_gap.try_send(());
        }
        gap |= info.discontinuity;
    });
    let cap = discovery.open_capture(browser, craft_app_audio::Target { pid: 41 }, on_data).unwrap();
    assert!(gap_counted.recv_timeout(wait).is_ok(), "the gap arrived");
    let status = run(&mut e, "app_audio.status", json!({})).unwrap();
    assert_eq!(status.result["discontinuities"], 1, "{}", status.result);
    assert_eq!(status.result["running"][0]["discontinuities"], 1, "{}", status.result);
    drop(cap);
    assert!(run(&mut e, "app_audio.list", json!({"app": "x"})).is_err(), "the list takes no parameters");
    fake.set_support(craft_app_audio::Support::no("needs a newer system"));
    let none = run(&mut e, "app_audio.list", json!({})).unwrap();
    assert_eq!(none.result["supported"], false);
    assert_eq!(none.result["error"], "needs a newer system");
    assert_eq!(none.result["apps"], json!([]));
    assert_eq!(e.revision(), rev);
    assert!(!e.is_dirty());
    assert_eq!(e.undo_label(), undo);
}

/// An application audio source whose `app` choices are `apps`.
struct TestAppAudio(Vec<String>);
impl Plugin for TestAppAudio {
    fn id(&self) -> &'static str {
        crate::lanes::APP_AUDIO_KIND
    }
    fn name(&self) -> &'static str {
        "Application audio capture"
    }
    fn schema(&self) -> SettingsSchema {
        let mut options = vec![studiocraft_model::ChoiceOption::new("", "Choose an application")];
        options.extend(self.0.iter().map(|a| studiocraft_model::ChoiceOption::new(a.clone(), a.clone())));
        SettingsSchema::new(vec![Field::new("app", "Application", FieldKind::Choice { options }, "")])
    }
}
impl Source for TestAppAudio {
    fn caps(&self) -> SourceCaps {
        SourceCaps { video: false, audio: true }
    }
    fn create(&self, _: &Settings, _: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        Err(PluginError::Unavailable("test".into()))
    }
}

const PLAYER: &str = r"player.exe|c:\apps\player.exe";
const BROWSER: &str = r"browser.exe|c:\apps\browser.exe";
/// A value the test source's settings accept, as the real source keeps a saved choice, for an
/// application that is not running.
const EDITOR: &str = r"editor.exe|c:\apps\editor.exe";

/// An engine with a microphone and a scripted list holding a player that plays at half level and
/// a browser, once the list has been read.
fn lane_engine() -> (Engine, craft_app_audio::fake::FakeBackend, craft_app_audio::Discovery) {
    use craft_app_audio::fake::{FakeBackend, app};
    let mut r = Registry::new();
    r.register_source(Arc::new(TestSource("color", SourceCaps { video: true, audio: false }))).unwrap();
    r.register_source(Arc::new(TestSource(crate::lanes::MIC_KIND, SourceCaps { video: false, audio: true }))).unwrap();
    r.register_source(Arc::new(TestAppAudio(vec![PLAYER.into(), BROWSER.into(), EDITOR.into()]))).unwrap();
    let mut c = SceneCollection::new("Test", "Scene");
    let mic = SourceId(c.alloc_id());
    c.sources.push(SourceConfig {
        id: mic,
        kind: crate::lanes::MIC_KIND.into(),
        kind_version: 1,
        name: "Microphone".into(),
        settings: Settings::new(),
        filters: vec![],
        audio: Default::default(),
    });
    c.global_audio.push(mic);
    let fake = FakeBackend::new();
    let player = app("player.exe", 40, true);
    fake.set_peak(&player.key, 0.5);
    fake.set_apps(vec![player, app("browser.exe", 41, false)]);
    let mut e = Engine::new(c, Arc::new(r));
    let discovery = craft_app_audio::Discovery::new(Arc::new(fake.clone()));
    e.set_app_audio(discovery.clone());
    let wait = std::time::Duration::from_secs(10);
    e.app_audio_lanes();
    assert!(discovery.fresh_list(wait).is_some());
    let passes = discovery.peak_passes();
    assert!(discovery.wait_peak_passes(passes + 1, wait));
    (e, fake, discovery)
}

fn lane<'a>(lanes: &'a serde_json::Value, name: &str) -> &'a serde_json::Value {
    lanes["lanes"].as_array().and_then(|l| l.iter().find(|l| l["name"] == name)).unwrap_or_else(|| panic!("no lane {name}: {lanes}"))
}

#[test]
fn the_lanes_list_microphones_then_applications_and_gray_those_not_heard() {
    let (mut e, _fake, _discovery) = lane_engine();
    let (rev, undo) = (e.revision(), e.undo_label());
    let o = run(&mut e, "app_audio.lanes", json!({})).unwrap();
    assert!(!o.changed && o.runtime.is_none());
    let names: Vec<&str> = o.result["lanes"].as_array().unwrap().iter().filter_map(|l| l["name"].as_str()).collect();
    assert_eq!(names, ["Microphone", "browser", "player"], "{}", o.result);
    let mic = lane(&o.result, "Microphone");
    assert_eq!(
        (mic["kind"].as_str(), mic["routed"].as_bool(), mic["heard"].as_bool(), mic["peak"].is_null()),
        (Some("microphone"), Some(true), Some(true), true)
    );
    let player = lane(&o.result, "player");
    assert_eq!(player["kind"], "application");
    assert_eq!(player["app"], PLAYER);
    assert_eq!((player["routed"].as_bool(), player["heard"].as_bool()), (Some(false), Some(false)), "an application is not heard until it is routed");
    assert_eq!(player["source"], serde_json::Value::Null);
    assert_eq!(player["peak"], 0.5, "its level is read without capturing it: {}", o.result);
    assert_eq!((player["running"].as_bool(), player["playing"].as_bool()), (Some(true), Some(true)));
    assert_eq!(lane(&o.result, "browser")["playing"], false);
    assert_eq!(o.result["supported"], true);
    assert_eq!(e.revision(), rev);
    assert_eq!(e.undo_label(), undo);
    assert!(run(&mut e, "app_audio.lanes", json!({"app": PLAYER})).is_err(), "the lanes take no parameters");
}

#[test]
fn routing_an_application_mixes_it_with_the_microphone_in_every_scene() {
    let (mut e, fake, discovery) = lane_engine();
    let o = run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": true})).unwrap();
    assert!(o.changed);
    let id: SourceId = serde_json::from_value(o.result["source"].clone()).unwrap();
    let c = e.collection();
    let s = c.source(id).unwrap();
    assert_eq!((s.kind.as_str(), s.name.as_str(), s.settings.str("app")), (crate::lanes::APP_AUDIO_KIND, "player", Some(PLAYER)));
    assert_eq!(c.global_audio.len(), 2, "the microphone stays: {:?}", c.global_audio);
    assert!(c.global_audio.contains(&id));
    assert_eq!(e.undo_label(), Some("Route Application Audio"));
    let lanes = run(&mut e, "app_audio.lanes", json!({})).unwrap().result;
    let player = lane(&lanes, "player");
    assert_eq!((player["routed"].as_bool(), player["heard"].as_bool(), player["source"].as_u64()), (Some(true), Some(true), Some(id.0)), "{lanes}");
    // Routing it again changes nothing and adds no undo step.
    let rev = e.revision();
    let again = run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": true})).unwrap();
    assert!(!again.changed);
    assert_eq!(again.result["source"].as_u64(), Some(id.0));
    assert_eq!(e.revision(), rev);
    run(&mut e, "edit.undo", json!({})).unwrap();
    assert!(e.collection().source(id).is_none(), "undo takes the routing back");
    assert_eq!(e.undo_label(), None);
    run(&mut e, "edit.redo", json!({})).unwrap();
    // The application quits: its lane stays, routed and not running, so it can be unrouted.
    let generation = discovery.generation();
    fake.set_apps(vec![craft_app_audio::fake::app("browser.exe", 41, false)]);
    assert_eq!(discovery.wait_newer(generation, std::time::Duration::from_secs(10)).apps.len(), 1);
    let lanes = run(&mut e, "app_audio.lanes", json!({})).unwrap().result;
    let gone = lane(&lanes, "player");
    assert_eq!((gone["running"].as_bool(), gone["routed"].as_bool(), gone["app"].as_str()), (Some(false), Some(true), Some(PLAYER)), "{lanes}");
    let off = run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": false})).unwrap();
    assert!(off.changed);
    assert_eq!(off.result["sources"], json!([id.0]));
    assert!(e.collection().source(id).is_none(), "a source no scene shows is removed");
    assert_eq!(e.collection().global_audio.len(), 1);
    let lanes = run(&mut e, "app_audio.lanes", json!({})).unwrap().result;
    assert!(lanes["lanes"].as_array().unwrap().iter().all(|l| l["name"] != "player"), "{lanes}");
    // With no source left and the application not running, the value names nothing.
    let rev = e.revision();
    assert!(run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": false})).is_err());
    assert_eq!(e.revision(), rev);
    // A listed application that is not routed unroutes without a change.
    let off = run(&mut e, "app_audio.route", json!({"app": BROWSER, "routed": false})).unwrap();
    assert!(!off.changed);
    assert_eq!(off.result["sources"], json!([]));
    assert_eq!(e.revision(), rev);
}

#[test]
fn an_applications_level_is_read_on_its_own_between_lane_reads() {
    let (e, fake, discovery) = lane_engine();
    let player = craft_app_audio::AppKey::decode(PLAYER).unwrap();
    assert_eq!(e.app_audio_peak(&player), Some(0.5));
    fake.set_peak(&craft_app_audio::fake::app("player.exe", 40, true).key, 0.25);
    let passes = discovery.peak_passes();
    assert!(discovery.wait_peak_passes(passes + 2, std::time::Duration::from_secs(10)));
    assert_eq!(e.app_audio_peak(&player), Some(0.25));
    assert_eq!(e.app_audio_peak(&craft_app_audio::AppKey::decode(EDITOR).unwrap()), None, "an application that is not listed");
}

#[test]
fn two_applications_whose_captures_overlap_are_not_routed_together() {
    let (mut e, fake, discovery) = lane_engine();
    // The player runs inside the browser's process tree, so the browser's capture records it.
    let mut player = craft_app_audio::fake::app("player.exe", 40, true);
    player.within = Some(BROWSER.into());
    let generation = discovery.generation();
    fake.set_apps(vec![player, craft_app_audio::fake::app("browser.exe", 41, false)]);
    assert!(discovery.wait_newer(generation, std::time::Duration::from_secs(10)).apps.iter().any(|a| a.within.is_some()));
    for (first, second) in [(BROWSER, PLAYER), (PLAYER, BROWSER)] {
        run(&mut e, "app_audio.route", json!({"app": first, "routed": true})).unwrap();
        let before = e.collection().clone();
        assert!(run(&mut e, "app_audio.route", json!({"app": second, "routed": true})).is_err(), "{second} after {first}");
        assert_eq!(e.collection(), &before);
        assert!(!run(&mut e, "app_audio.route", json!({"app": first, "routed": true})).unwrap().changed, "routing {first} again is allowed");
        let err = run(&mut e, "app_audio.route", json!({"app": second, "routed": true})).err().map(|e| e.to_string()).unwrap_or_default();
        assert!(err.contains("stop routing"), "{err}");
        run(&mut e, "app_audio.route", json!({"app": first, "routed": false})).unwrap();
    }
    // The browser heard only through the program scene: hiding it is the remedy.
    run(&mut e, "source.add", json!({"kind": crate::lanes::APP_AUDIO_KIND, "name": "Browser", "settings": {"app": BROWSER}})).unwrap();
    let err = run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": true})).err().map(|e| e.to_string()).unwrap_or_default();
    assert!(err.contains("hide browser's source"), "{err}");
    // Routed and shown in the program scene: both changes are named.
    run(&mut e, "app_audio.route", json!({"app": BROWSER, "routed": true})).unwrap();
    let err = run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": true})).err().map(|e| e.to_string()).unwrap_or_default();
    assert!(err.contains("stop routing browser and hide its source"), "{err}");
}

#[test]
fn routing_uses_the_source_the_program_scene_already_plays() {
    let (mut e, _fake, _discovery) = lane_engine();
    // A source no scene shows comes first in the collection; a later one is shown in the scene.
    let mut c = e.collection().clone();
    let spare = SourceId(c.alloc_id());
    c.sources.push(SourceConfig {
        id: spare,
        kind: crate::lanes::APP_AUDIO_KIND.into(),
        kind_version: 1,
        name: "Spare".into(),
        settings: Settings::new().with("app", PLAYER),
        filters: vec![],
        audio: Default::default(),
    });
    e.load(c);
    let o = run(&mut e, "source.add", json!({"kind": crate::lanes::APP_AUDIO_KIND, "name": "Shown", "settings": {"app": PLAYER}})).unwrap();
    let shown: SourceId = serde_json::from_value(o.result["source"].clone()).unwrap();
    let r = run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": true})).unwrap();
    assert_eq!(r.result["source"].as_u64(), Some(shown.0), "no second capture of what the scene plays");
    assert!(!e.collection().global_audio.contains(&spare));
}

#[test]
fn turning_on_a_monitor_only_route_checks_for_overlap_too() {
    let (mut e, fake, discovery) = lane_engine();
    let mut player = craft_app_audio::fake::app("player.exe", 40, true);
    player.within = Some(BROWSER.into());
    let generation = discovery.generation();
    fake.set_apps(vec![player, craft_app_audio::fake::app("browser.exe", 41, false)]);
    assert!(discovery.wait_newer(generation, std::time::Duration::from_secs(10)).apps.iter().any(|a| a.within.is_some()));
    let mut c = e.collection().clone();
    let quiet = SourceId(c.alloc_id());
    c.sources.push(SourceConfig {
        id: quiet,
        kind: crate::lanes::APP_AUDIO_KIND.into(),
        kind_version: 1,
        name: "Player".into(),
        settings: Settings::new().with("app", PLAYER),
        filters: vec![],
        audio: studiocraft_model::AudioState { monitor: studiocraft_model::MonitorMode::MonitorOnly, ..Default::default() },
    });
    c.global_audio.push(quiet);
    e.load(c);
    run(&mut e, "app_audio.route", json!({"app": BROWSER, "routed": true})).unwrap();
    let before = e.collection().clone();
    assert!(run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": true})).is_err());
    assert_eq!(e.collection(), &before, "the monitor-only source stays out of the program");
}

#[test]
fn an_application_two_levels_inside_a_routed_one_is_not_routed() {
    let (mut e, fake, discovery) = lane_engine();
    // The player runs inside the browser, and the browser inside the editor.
    let mut player = craft_app_audio::fake::app("player.exe", 40, true);
    player.within = Some(BROWSER.into());
    let mut browser = craft_app_audio::fake::app("browser.exe", 41, false);
    browser.within = Some(EDITOR.into());
    let generation = discovery.generation();
    fake.set_apps(vec![player, browser, craft_app_audio::fake::app("editor.exe", 42, false)]);
    assert_eq!(discovery.wait_newer(generation, std::time::Duration::from_secs(10)).apps.len(), 3);
    for (first, second) in [(EDITOR, PLAYER), (PLAYER, EDITOR)] {
        run(&mut e, "app_audio.route", json!({"app": first, "routed": true})).unwrap();
        assert!(run(&mut e, "app_audio.route", json!({"app": second, "routed": true})).is_err(), "{second} after {first}");
        run(&mut e, "app_audio.route", json!({"app": first, "routed": false})).unwrap();
    }
}

#[test]
fn routing_a_source_set_to_monitor_only_puts_it_in_the_program() {
    let (mut e, _fake, _discovery) = lane_engine();
    let id: SourceId =
        serde_json::from_value(run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": true})).unwrap().result["source"].clone()).unwrap();
    run(&mut e, "source.monitor", json!({"source": id.0, "mode": "monitor_only"})).unwrap();
    let player = lane(&run(&mut e, "app_audio.lanes", json!({})).unwrap().result, "player").clone();
    assert_eq!((player["routed"].as_bool(), player["heard"].as_bool()), (Some(true), Some(false)));
    let r = run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": true})).unwrap();
    assert!(r.changed);
    assert_eq!(e.collection().source(id).map(|s| s.audio.monitor), Some(studiocraft_model::MonitorMode::MonitorAndOutput));
    let player = lane(&run(&mut e, "app_audio.lanes", json!({})).unwrap().result, "player").clone();
    assert_eq!(player["heard"], true);
}

#[test]
fn a_lane_with_two_sources_acts_on_the_one_the_program_hears() {
    let (mut e, _fake, _discovery) = lane_engine();
    let o = run(&mut e, "source.add", json!({"kind": crate::lanes::APP_AUDIO_KIND, "name": "Shown", "settings": {"app": PLAYER}})).unwrap();
    let shown: SourceId = serde_json::from_value(o.result["source"].clone()).unwrap();
    let mut c = e.collection().clone();
    let quiet = SourceId(c.alloc_id());
    let audio = studiocraft_model::AudioState { monitor: studiocraft_model::MonitorMode::MonitorOnly, ..Default::default() };
    c.sources.push(SourceConfig {
        id: quiet,
        kind: crate::lanes::APP_AUDIO_KIND.into(),
        kind_version: 1,
        name: "Monitor".into(),
        settings: Settings::new().with("app", PLAYER),
        filters: vec![],
        audio,
    });
    c.global_audio.push(quiet);
    e.load(c);
    let player = lane(&run(&mut e, "app_audio.lanes", json!({})).unwrap().result, "player").clone();
    assert_eq!(player["source"].as_u64(), Some(shown.0), "the scene's source reaches the program: {player}");
    assert_eq!((player["routed"].as_bool(), player["heard"].as_bool()), (Some(true), Some(true)));
    assert_eq!(player["routed_heard"].as_bool(), Some(false), "the routed source is monitor only");
    run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": true})).unwrap();
    let player = lane(&run(&mut e, "app_audio.lanes", json!({})).unwrap().result, "player").clone();
    assert_eq!(player["routed_heard"].as_bool(), Some(true), "routing again puts a routed source in the program: {player}");
}

fn add_app_source(c: &mut SceneCollection, name: &str, app: &str, monitor: studiocraft_model::MonitorMode) -> SourceId {
    let id = SourceId(c.alloc_id());
    c.sources.push(SourceConfig {
        id,
        kind: crate::lanes::APP_AUDIO_KIND.into(),
        kind_version: 1,
        name: name.into(),
        settings: Settings::new().with("app", app),
        filters: vec![],
        audio: studiocraft_model::AudioState { monitor, ..Default::default() },
    });
    c.global_audio.push(id);
    id
}

#[test]
fn the_lanes_say_whether_the_program_hears_desktop_audio() {
    let mut c = SceneCollection::new("Test", "Scene");
    assert!(!crate::lanes::desktop_heard(&c));
    let id = SourceId(c.alloc_id());
    c.sources.push(SourceConfig {
        id,
        kind: crate::lanes::DESKTOP_KIND.into(),
        kind_version: 1,
        name: "Desktop audio".into(),
        settings: Settings::new(),
        filters: vec![],
        audio: Default::default(),
    });
    assert!(!crate::lanes::desktop_heard(&c), "a source no scene shows and not global");
    c.global_audio.push(id);
    assert!(crate::lanes::desktop_heard(&c));
    if let Some(s) = c.sources.iter_mut().find(|s| s.id == id) {
        s.audio.muted = true;
    }
    assert!(!crate::lanes::desktop_heard(&c), "muted");
}

#[test]
fn a_second_source_the_program_hears_gets_a_lane_of_its_own() {
    let (mut e, _fake, _discovery) = lane_engine();
    let mut c = e.collection().clone();
    let one = add_app_source(&mut c, "Player one", PLAYER, studiocraft_model::MonitorMode::MonitorAndOutput);
    let two = add_app_source(&mut c, "Player two", PLAYER, studiocraft_model::MonitorMode::MonitorAndOutput);
    e.load(c);
    let r = run(&mut e, "app_audio.lanes", json!({})).unwrap().result;
    let sources: Vec<(Option<u64>, bool)> = r["lanes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|l| l["kind"] == "application" && l["source"].is_u64())
        .map(|l| (l["source"].as_u64(), l["app"].is_null()))
        .collect();
    assert_eq!(sources, vec![(Some(one.0), false), (Some(two.0), true)], "its mute and volume act on that source: {r}");
    for field in ["playing", "peak", "within"] {
        assert_eq!(lane(&r, "Player two")[field], lane(&r, "player")[field], "the application's {field} is the same on both");
    }
}

#[test]
fn a_quit_applications_lane_keeps_the_route_key_with_a_path() {
    let (mut e, fake, discovery) = lane_engine();
    let mut c = e.collection().clone();
    add_app_source(&mut c, "Game", r"game.exe|c:\a\game.exe", studiocraft_model::MonitorMode::MonitorOnly);
    add_app_source(&mut c, "Game anywhere", "game.exe", studiocraft_model::MonitorMode::MonitorAndOutput);
    e.load(c);
    let generation = discovery.generation();
    fake.set_apps(vec![]);
    assert!(discovery.wait_newer(generation, std::time::Duration::from_secs(10)).apps.is_empty());
    let r = run(&mut e, "app_audio.lanes", json!({})).unwrap().result;
    assert_eq!(lane(&r, "Game anywhere")["app"], r"game.exe|c:\a\game.exe", "{r}");
}

#[test]
fn a_quit_program_at_each_path_keeps_a_lane_of_its_own() {
    let (mut e, fake, discovery) = lane_engine();
    let mut c = e.collection().clone();
    add_app_source(&mut c, "Game anywhere", "game.exe", studiocraft_model::MonitorMode::MonitorAndOutput);
    add_app_source(&mut c, "Game A", r"game.exe|c:\a\game.exe", studiocraft_model::MonitorMode::MonitorAndOutput);
    add_app_source(&mut c, "Game B", r"game.exe|c:\b\game.exe", studiocraft_model::MonitorMode::MonitorAndOutput);
    e.load(c);
    let generation = discovery.generation();
    fake.set_apps(vec![]);
    assert!(discovery.wait_newer(generation, std::time::Duration::from_secs(10)).apps.is_empty());
    let r = run(&mut e, "app_audio.lanes", json!({})).unwrap().result;
    let apps: Vec<&str> = r["lanes"].as_array().into_iter().flatten().filter_map(|l| l["app"].as_str()).collect();
    assert_eq!(apps, vec![r"game.exe|c:\a\game.exe", r"game.exe|c:\b\game.exe", "game.exe"], "{r}");
    // Each lane's key unroutes that lane's sources and no other.
    let saved = e.collection().clone();
    run(&mut e, "app_audio.route", json!({"app": "game.exe", "routed": false})).unwrap();
    let names: Vec<&str> = e.collection().sources.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["Microphone", "Game A", "Game B"]);
    e.load(saved);
    run(&mut e, "app_audio.route", json!({"app": r"game.exe|c:\b\game.exe", "routed": false})).unwrap();
    let names: Vec<&str> = e.collection().sources.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["Microphone", "Game anywhere", "Game A"]);
    // With one path left, the file-name source shares its lane and its key.
    let r = run(&mut e, "app_audio.lanes", json!({})).unwrap().result;
    let apps: Vec<&str> = r["lanes"].as_array().into_iter().flatten().filter_map(|l| l["app"].as_str()).collect();
    assert_eq!(apps, vec![r"game.exe|c:\a\game.exe"], "{r}");
    run(&mut e, "app_audio.route", json!({"app": r"game.exe|c:\a\game.exe", "routed": false})).unwrap();
    assert_eq!(e.collection().sources.len(), 1);
}

#[test]
fn a_quit_applications_lane_shows_the_routed_source_the_program_hears() {
    let (mut e, fake, discovery) = lane_engine();
    let mut c = e.collection().clone();
    add_app_source(&mut c, "Quiet", PLAYER, studiocraft_model::MonitorMode::MonitorOnly);
    let loud = add_app_source(&mut c, "Loud", PLAYER, studiocraft_model::MonitorMode::MonitorAndOutput);
    e.load(c);
    let generation = discovery.generation();
    fake.set_apps(vec![]);
    assert!(discovery.wait_newer(generation, std::time::Duration::from_secs(10)).apps.is_empty());
    let r = run(&mut e, "app_audio.lanes", json!({})).unwrap().result;
    let l = lane(&r, "Loud");
    assert_eq!(l["source"].as_u64(), Some(loud.0), "{r}");
    assert_eq!((l["heard"].as_bool(), l["routed_heard"].as_bool()), (Some(true), Some(true)));
    assert!(r["lanes"].as_array().is_some_and(|ls| !ls.iter().any(|l| l["name"] == "Quiet")), "one lane for the application: {r}");
}

#[test]
fn a_source_saved_before_the_program_moved_is_its_lane_and_is_routed_again() {
    let (mut e, _fake, _discovery) = lane_engine();
    let mut c = e.collection().clone();
    let old = SourceId(c.alloc_id());
    c.sources.push(SourceConfig {
        id: old,
        kind: crate::lanes::APP_AUDIO_KIND.into(),
        kind_version: 1,
        name: "Player".into(),
        settings: Settings::new().with("app", r"player.exe|c:\old\player.exe"),
        filters: vec![],
        audio: Default::default(),
    });
    e.load(c);
    let player = lane(&run(&mut e, "app_audio.lanes", json!({})).unwrap().result, "player").clone();
    assert_eq!(player["source"].as_u64(), Some(old.0), "the capture source follows the program to its new folder: {player}");
    let r = run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": true})).unwrap();
    assert_eq!(r.result["source"].as_u64(), Some(old.0), "no second capture of the same program");
    assert_eq!(e.collection().sources.iter().filter(|s| s.kind == crate::lanes::APP_AUDIO_KIND).count(), 1);
}

#[test]
fn unrouting_a_program_stops_every_capture_of_it_and_keeps_a_source_saved_for_another_folder() {
    let (mut e, _fake, _discovery) = lane_engine();
    // The player is listed at c:\apps; a second source was saved for a copy in another folder.
    let mut c = e.collection().clone();
    let mut ids = Vec::new();
    for (name, app) in [("Here", PLAYER), ("Old copy", r"player.exe|c:\old\player.exe")] {
        let id = SourceId(c.alloc_id());
        c.sources.push(SourceConfig {
            id,
            kind: crate::lanes::APP_AUDIO_KIND.into(),
            kind_version: 1,
            name: name.into(),
            settings: Settings::new().with("app", app),
            filters: vec![],
            audio: Default::default(),
        });
        c.global_audio.push(id);
        ids.push(id);
    }
    e.load(c);
    // Both capture the listed player, so the program stops hearing it through either.
    let off = run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": false})).unwrap();
    assert_eq!(off.result["sources"], json!([ids[0].0, ids[1].0]));
    assert!(e.collection().source(ids[0]).is_none(), "the source for the player itself is removed");
    assert!(e.collection().source(ids[1]).is_some(), "the source saved for the other folder is kept");
    assert!(e.collection().global_audio.iter().all(|g| !ids.contains(g)));
}

#[test]
fn a_file_name_that_matches_two_unlisted_programs_routes_neither() {
    let (mut e, fake, discovery) = lane_engine();
    let mut c = e.collection().clone();
    for (name, path) in [("Game A", r"game.exe|c:\a\game.exe"), ("Game B", r"game.exe|c:\b\game.exe")] {
        let id = SourceId(c.alloc_id());
        c.sources.push(SourceConfig {
            id,
            kind: crate::lanes::APP_AUDIO_KIND.into(),
            kind_version: 1,
            name: name.into(),
            settings: Settings::new().with("app", path),
            filters: vec![],
            audio: Default::default(),
        });
        c.global_audio.push(id);
    }
    e.load(c);
    let generation = discovery.generation();
    fake.set_apps(vec![]);
    assert!(discovery.wait_newer(generation, std::time::Duration::from_secs(10)).apps.is_empty());
    let before = e.collection().clone();
    for routed in [false, true] {
        assert!(run(&mut e, "app_audio.route", json!({"app": "game.exe", "routed": routed})).is_err(), "routed: {routed}");
    }
    assert_eq!(e.collection(), &before);
    run(&mut e, "app_audio.route", json!({"app": r"game.exe|c:\a\game.exe", "routed": false})).unwrap();
    let names: Vec<&str> = e.collection().sources.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"Game B") && !names.contains(&"Game A"), "only the program at its path is unrouted: {names:?}");
}

#[test]
fn routing_reuses_a_source_a_scene_shows_and_unrouting_leaves_it_in_its_scene() {
    let (mut e, _fake, _discovery) = lane_engine();
    let o = run(&mut e, "source.add", json!({"kind": crate::lanes::APP_AUDIO_KIND, "name": "Game", "settings": {"app": BROWSER}})).unwrap();
    let id: SourceId = serde_json::from_value(o.result["source"].clone()).unwrap();
    let item: ItemId = serde_json::from_value(o.result["item"].clone()).unwrap();
    let browser = |e: &mut Engine| lane(&run(e, "app_audio.lanes", json!({})).unwrap().result, "browser").clone();
    let state = |l: &serde_json::Value| (l["routed"].as_bool(), l["heard"].as_bool());
    assert_eq!(state(&browser(&mut e)), (Some(false), Some(true)), "a source the program scene shows is heard without being routed");
    run(&mut e, "item.visible", json!({"item": item, "visible": false})).unwrap();
    let l = browser(&mut e);
    assert_eq!(state(&l), (Some(false), Some(false)), "a hidden item is not heard");
    assert_eq!(l["source"].as_u64(), Some(id.0), "the lane carries the scene's source");
    let r = run(&mut e, "app_audio.route", json!({"app": BROWSER, "routed": true})).unwrap();
    assert_eq!(r.result["source"].as_u64(), Some(id.0), "the scene's source is routed, not a new one");
    assert_eq!(e.collection().sources.iter().filter(|s| s.kind == crate::lanes::APP_AUDIO_KIND).count(), 1);
    assert_eq!(state(&browser(&mut e)), (Some(true), Some(true)));
    run(&mut e, "app_audio.route", json!({"app": BROWSER, "routed": false})).unwrap();
    assert!(e.collection().source(id).is_some(), "the scene still shows it");
    assert!(!e.collection().global_audio.contains(&id));
    run(&mut e, "item.visible", json!({"item": item, "visible": true})).unwrap();
    assert_eq!(state(&browser(&mut e)), (Some(false), Some(true)), "unrouted, the scene's item is still heard");
}

#[test]
fn a_lane_monitored_only_is_not_heard_by_the_program() {
    let (mut e, _fake, _discovery) = lane_engine();
    run(&mut e, "source.monitor", json!({"source": "Microphone", "mode": "monitor_only"})).unwrap();
    let mic = lane(&run(&mut e, "app_audio.lanes", json!({})).unwrap().result, "Microphone").clone();
    assert_eq!((mic["routed"].as_bool(), mic["heard"].as_bool()), (Some(true), Some(false)), "{mic}");
    run(&mut e, "source.monitor", json!({"source": "Microphone", "mode": "monitor_and_output"})).unwrap();
    let mic = lane(&run(&mut e, "app_audio.lanes", json!({})).unwrap().result, "Microphone").clone();
    assert_eq!(mic["heard"], true);
}

#[test]
fn unrouting_removes_only_the_applications_own_sources() {
    let (mut e, _fake, _discovery) = lane_engine();
    // A loaded collection can hold a source that no scene shows; unrouting leaves it alone.
    let mut c = e.collection().clone();
    let spare = SourceId(c.alloc_id());
    c.sources.push(SourceConfig {
        id: spare,
        kind: "color".into(),
        kind_version: 1,
        name: "Spare".into(),
        settings: Settings::new(),
        filters: vec![],
        audio: Default::default(),
    });
    e.load(c);
    let r = run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": true})).unwrap();
    let id: SourceId = serde_json::from_value(r.result["source"].clone()).unwrap();
    run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": false})).unwrap();
    assert!(e.collection().source(id).is_none(), "the routed source no scene shows is removed");
    assert!(e.collection().source(spare).is_some(), "another source no scene shows stays");
}

#[test]
fn a_route_to_something_that_is_not_an_application_is_refused() {
    let (mut e, _fake, _discovery) = lane_engine();
    let before = e.collection().clone();
    // Empty, a name with a folder in it, and applications that are not running: one by its file
    // name alone, and one whose value the source's settings accept.
    let bad = [
        json!({"app": "", "routed": true}),
        json!({"app": "dir/player.exe", "routed": true}),
        json!({"app": "editor.exe", "routed": true}),
        json!({"app": EDITOR, "routed": true}),
    ];
    for bad in bad {
        assert!(run(&mut e, "app_audio.route", bad.clone()).is_err(), "{bad}");
    }
    assert!(run(&mut e, "app_audio.route", json!({"app": PLAYER})).is_err(), "`routed` is required");
    assert!(run(&mut e, "app_audio.route", json!({"app": PLAYER, "routed": "yes"})).is_err());
    assert_eq!(e.collection(), &before);
    assert_eq!(e.undo_label(), None);
}

/// A source whose `mode` choices depend on its `device`.
struct DeviceSource;
impl Plugin for DeviceSource {
    fn id(&self) -> &'static str {
        "device"
    }
    fn name(&self) -> &'static str {
        "Device"
    }
    fn schema(&self) -> SettingsSchema {
        self.schema_for(&Settings::new())
    }
    fn schema_for(&self, s: &Settings) -> SettingsSchema {
        let mut modes = vec![studiocraft_model::ChoiceOption::new("", "Default")];
        if s.str("device") == Some("b") {
            modes.push(studiocraft_model::ChoiceOption::new("hd", "HD"));
        }
        SettingsSchema::new(vec![
            Field::new("device", "Device", FieldKind::Choice { options: vec![studiocraft_model::ChoiceOption::new("b", "B")] }, ""),
            Field::new("mode", "Mode", FieldKind::Choice { options: modes }, ""),
        ])
    }
}
impl Source for DeviceSource {
    fn caps(&self) -> SourceCaps {
        SourceCaps { video: true, audio: false }
    }
    fn create(&self, _: &Settings, _: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
        Err(PluginError::Unavailable("test".into()))
    }
}

#[test]
fn settings_are_checked_against_the_schema_for_those_settings() {
    let mut r = Registry::new();
    r.register_source(Arc::new(DeviceSource)).unwrap();
    let mut e = Engine::new(SceneCollection::new("Test", "Scene"), Arc::new(r));
    assert!(run(&mut e, "source.add", json!({"kind": "device", "settings": {"mode": "hd"}})).is_err(), "no device offers hd");
    let o = run(&mut e, "source.add", json!({"kind": "device", "name": "Cam", "settings": {"device": "b", "mode": "hd"}})).unwrap();
    assert!(o.changed);
    run(&mut e, "source.settings", json!({"source": "Cam", "settings": {"device": "b", "mode": ""}})).unwrap();
    assert!(run(&mut e, "source.settings", json!({"source": "Cam", "settings": {"device": "", "mode": "hd"}})).is_err());
    run(&mut e, "source.settings", json!({"source": "Cam", "settings": {"device": "b", "mode": "hd"}})).unwrap();
    let kept = run(&mut e, "source.settings", json!({"source": "Cam", "settings": {"device": "", "mode": "hd"}}));
    assert!(kept.is_err(), "a mode kept from another device is checked against the new one");
    let omitted = run(&mut e, "source.settings", json!({"source": "Cam", "settings": {"mode": "hd"}}));
    assert!(omitted.is_err(), "an omitted device becomes the default one, which offers no hd");
    let schema = run(&mut e, "source.schema", json!({"kind": "device", "settings": {"device": "b"}})).unwrap();
    assert_eq!(schema.result["schema"]["fields"][1]["kind"]["options"][1]["value"], "hd", "{}", schema.result);
}

#[test]
fn unknown_references_are_errors() {
    let mut e = engine();
    assert!(matches!(run(&mut e, "scene.rename", json!({"scene": 99, "name": "x"})), Err(EngineError::NoScene(_))));
    assert!(matches!(run(&mut e, "source.rename", json!({"source": "ghost", "name": "x"})), Err(EngineError::NoSource(_))));
    assert!(matches!(run(&mut e, "item.remove", json!({"item": 1234})), Err(EngineError::NoItem(_))));
    assert!(e.state_json()["collection"]["scenes"].is_array());
}

fn staging_engine() -> Engine {
    let mut r = Registry::new();
    r.register_source(Arc::new(TestSource("color", SourceCaps { video: true, audio: false }))).unwrap();
    r.register_source(Arc::new(TestSource(staged::KIND, SourceCaps { video: true, audio: false }))).unwrap();
    Engine::new(SceneCollection::new("Test", "Scene"), Arc::new(r))
}

fn presented(e: &Engine, name: &str) -> Option<i64> {
    e.collection().source_by_name(name).and_then(|s| s.settings.i64(staged::PRESENTED))
}

#[test]
fn present_commands_step_through_the_staged_windows() {
    let mut e = staging_engine();
    let slots = json!({"window1": "a.exe\u{1f}A", "window3": "b.exe\u{1f}B", "window6": "c.exe\u{1f}C", "presented": 1});
    run(&mut e, "source.add", json!({"kind": staged::KIND, "name": "Stage", "settings": slots})).unwrap();
    run(&mut e, "source.add", json!({"kind": staged::KIND, "name": "Other", "settings": {"window2": "d.exe\u{1f}D", "presented": 2}})).unwrap();
    add_color(&mut e, "Plain");
    let history = e.undo_label();

    let o = run(&mut e, "present.next", json!({"source": "Stage"})).unwrap();
    assert!(o.changed);
    assert_eq!(o.result["presented"][0]["slot"], 3, "{}", o.result);
    assert_eq!(presented(&e, "Stage"), Some(3), "the empty slot 2 is skipped");
    run(&mut e, "present.next", json!({"source": "Stage"})).unwrap();
    run(&mut e, "present.next", json!({"source": "Stage"})).unwrap();
    assert_eq!(presented(&e, "Stage"), Some(1), "it wraps around");
    run(&mut e, "present.previous", json!({"source": "Stage"})).unwrap();
    assert_eq!(presented(&e, "Stage"), Some(6));
    run(&mut e, "present.window", json!({"source": "Stage", "index": 3})).unwrap();
    assert_eq!(presented(&e, "Stage"), Some(3));
    assert_eq!(presented(&e, "Other"), Some(2), "a named source changes alone");
    assert_eq!(e.undo_label(), history, "presenting a window is not an undo step");
    assert!(e.is_dirty(), "the presented window is saved with the collection");

    // Without a source, every Staged windows source that has a window in the slot changes.
    let o = run(&mut e, "present.window", json!({"index": 1})).unwrap();
    assert_eq!(o.result["presented"].as_array().map(Vec::len), Some(1), "{}", o.result);
    assert_eq!((presented(&e, "Stage"), presented(&e, "Other")), (Some(1), Some(2)));
    run(&mut e, "present.next", json!({})).unwrap();
    assert_eq!((presented(&e, "Stage"), presented(&e, "Other")), (Some(3), Some(2)));
    let same = run(&mut e, "present.window", json!({"source": "Other", "index": 2})).unwrap();
    assert!(!same.changed, "presenting the presented window changes nothing");
    assert_eq!(same.result["presented"], json!([]), "and reports no source");
}

#[test]
fn present_commands_refuse_empty_slots_and_other_sources() {
    let mut e = staging_engine();
    assert_eq!(run(&mut e, "present.next", json!({})), Err(EngineError::Invalid("there is no Staged windows source".into())));
    run(&mut e, "source.add", json!({"kind": staged::KIND, "name": "Stage", "settings": {"window2": "a.exe\u{1f}A"}})).unwrap();
    add_color(&mut e, "Plain");
    assert_eq!(
        run(&mut e, "present.window", json!({"source": "Stage", "index": 4})),
        Err(EngineError::Invalid("nothing is staged in slot 4".into()))
    );
    assert_eq!(run(&mut e, "present.window", json!({"index": 4})), Err(EngineError::Invalid("nothing is staged in slot 4".into())));
    for bad in [0, 7] {
        let r = run(&mut e, "present.window", json!({"source": "Stage", "index": bad}));
        assert_eq!(r, Err(EngineError::Invalid("the slot must be a number from 1 to 6".into())), "{bad}");
    }
    assert!(run(&mut e, "present.window", json!({"source": "Stage", "index": -1})).is_err());
    assert!(run(&mut e, "present.window", json!({"source": "Stage", "index": "2"})).is_err());
    assert_eq!(run(&mut e, "present.next", json!({"source": "Plain"})), Err(EngineError::Invalid("`Plain` is not a Staged windows source".into())));
    assert!(matches!(run(&mut e, "present.next", json!({"source": "ghost"})), Err(EngineError::NoSource(_))));
    run(&mut e, "source.settings", json!({"source": "Stage", "settings": {"window2": ""}})).unwrap();
    assert_eq!(run(&mut e, "present.previous", json!({"source": "Stage"})), Err(EngineError::Invalid("nothing is staged".into())));
}

#[test]
fn undo_keeps_the_presented_window() {
    let mut e = staging_engine();
    run(&mut e, "source.add", json!({"kind": staged::KIND, "name": "Stage", "settings": {"window1": "a.exe\u{1f}A", "window2": "b.exe\u{1f}B"}}))
        .unwrap();
    add_color(&mut e, "Plain");
    run(&mut e, "present.window", json!({"source": "Stage", "index": 2})).unwrap();
    run(&mut e, "edit.undo", json!({})).unwrap();
    assert!(e.collection().source_by_name("Plain").is_none(), "the undo took the last edit back");
    assert_eq!(presented(&e, "Stage"), Some(2), "and left the presented window alone");

    // Presented window in Properties is no undo step either; a change to another setting is.
    let history = e.undo_label();
    let mut settings = e.collection().source_by_name("Stage").map(|s| s.settings.to_json()).unwrap_or_default();
    settings["presented"] = json!(1);
    run(&mut e, "source.settings", json!({"source": "Stage", "settings": settings})).unwrap();
    assert_eq!(presented(&e, "Stage"), Some(1));
    assert_eq!(e.undo_label(), history, "a change to the presented window alone");
    settings["window3"] = json!("c.exe\u{1f}C");
    run(&mut e, "source.settings", json!({"source": "Stage", "settings": settings})).unwrap();
    assert_ne!(e.undo_label(), history, "a change to a slot");
}

#[test]
fn every_spec_is_reachable() {
    // Each listed id parses with suitable parameters, so menus and scripts can reach all of them.
    for s in crate::command::specs() {
        let params = match s.id {
            "scene.remove" | "scene.select" => json!({"scene": 1}),
            "scene.rename" => json!({"scene": 1, "name": "x"}),
            "scene.duplicate" => json!({"scene": 1}),
            "scene.move" => json!({"scene": 1, "to": "up"}),
            "scene.select_index" => json!({"index": 1}),
            "scene.thumbnails" => json!({"scenes": [1]}),
            "source.add" | "source.schema" => json!({"kind": "color"}),
            "source.add_existing" => json!({"source": 1}),
            "source.rename" => json!({"source": 1, "name": "x"}),
            "source.settings" => json!({"source": 1, "settings": {}}),
            "source.volume" => json!({"source": 1, "gain": 0.5}),
            "source.mute" => json!({"source": 1, "muted": true}),
            "source.sync_offset" => json!({"source": 1, "ms": 100}),
            "source.monitor" => json!({"source": 1, "mode": "monitor_only"}),
            "source.set_record_track" => json!({"source": 1, "on": true}),
            "present.window" => json!({"index": 1}),
            "audio_tracks.repair" => json!({"folder": "x"}),
            "item.remove" | "item.duplicate" => json!({"item": 1}),
            "item.move" => json!({"item": 1, "to": "up"}),
            "item.transform" => json!({"item": 1, "transform": {}}),
            "canvas.relayout" => json!({"from": {"width": 1920, "height": 1080}, "to": {"width": 1280, "height": 720}}),
            "item.visible" => json!({"item": 1, "visible": true}),
            "item.lock" => json!({"item": 1, "locked": true}),
            "filter.add" => json!({"source": 1, "kind": "gain"}),
            "filter.remove" => json!({"source": 1, "filter": 1}),
            "filter.settings" => json!({"source": 1, "filter": 1, "settings": {}}),
            "filter.enable" => json!({"source": 1, "filter": 1, "enabled": true}),
            "filter.meter" => json!({"source": 1, "filter": 1}),
            "transition.set" => json!({"kind": "fade"}),
            "transition.duration" => json!({"ms": 100}),
            "transition.settings" => json!({"settings": {}}),
            "app_audio.route" => json!({"app": "player.exe", "routed": true}),
            "pause.extend" => json!({"seconds": 60}),
            "marker.rename" => json!({"index": 1, "name": "x"}),
            _ => json!({}),
        };
        let c = Command::from_parts(s.id, &params).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.id(), s.id);
        let _ = (SceneRef::Id(SceneId(1)), SourceRef::Id(SourceId(1)), ItemRef::Id(ItemId(1)), Move::Up, C::Undo {});
    }
}
