use std::sync::Arc;

use serde_json::json;
use studiocraft_model::{Field, FieldKind, Settings, SettingsSchema, Transform, Vec2};
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
fn volume_and_mute() {
    let mut e = engine();
    let o = run(&mut e, "source.add", json!({"kind": "mic", "name": "Mic"})).unwrap();
    let src: SourceId = serde_json::from_value(o.result["source"].clone()).unwrap();
    run(&mut e, "source.volume", json!({"source": "Mic", "gain": 0.25})).unwrap();
    for bad in [2.0, -0.1] {
        assert!(matches!(run(&mut e, "source.volume", json!({"source": "Mic", "gain": bad})), Err(EngineError::Invalid(_))), "{bad}");
    }
    assert_eq!(e.collection().source(src).unwrap().audio.gain, 0.25);
    run(&mut e, "source.mute", json!({"source": src, "muted": true})).unwrap();
    assert!(e.collection().source(src).unwrap().audio.muted);
    assert!(run(&mut e, "source.volume", json!({"source": "Mic", "gain": "loud"})).is_err());
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
        ("stream.start", RuntimeAction::StartStreaming),
        ("stream.stop", RuntimeAction::StopStreaming),
        ("stream.toggle", RuntimeAction::ToggleStreaming),
        ("replay.start", RuntimeAction::StartReplay),
        ("replay.stop", RuntimeAction::StopReplay),
        ("replay.toggle", RuntimeAction::ToggleReplay),
        ("replay.save", RuntimeAction::SaveReplay),
        ("audio_tracks.status", RuntimeAction::AudioTracksStatus),
        ("app.settings", RuntimeAction::OpenSettings),
        ("app.exit", RuntimeAction::Exit),
    ] {
        let o = run(&mut e, id, json!({})).unwrap();
        assert_eq!(o.runtime, Some(action), "{id}");
        assert!(!o.changed);
    }
    let o = run(&mut e, "audio_tracks.repair", json!({"folder": "D:/Videos"})).unwrap();
    assert_eq!(o.runtime, Some(RuntimeAction::AudioTracksRepair { folder: "D:/Videos".into() }));
    assert!(!o.changed);
    for bad in [json!({}), json!({"folder": ""}), json!({"folder": "  "}), json!({"folder": 3})] {
        assert!(run(&mut e, "audio_tracks.repair", bad.clone()).is_err(), "{bad}");
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
        run(&mut e, "app_audio.route", json!({"app": first, "routed": false})).unwrap();
    }
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
            "source.add" | "source.schema" => json!({"kind": "color"}),
            "source.add_existing" => json!({"source": 1}),
            "source.rename" => json!({"source": 1, "name": "x"}),
            "source.settings" => json!({"source": 1, "settings": {}}),
            "source.volume" => json!({"source": 1, "gain": 0.5}),
            "source.mute" => json!({"source": 1, "muted": true}),
            "source.sync_offset" => json!({"source": 1, "ms": 100}),
            "source.monitor" => json!({"source": 1, "mode": "monitor_only"}),
            "source.set_record_track" => json!({"source": 1, "on": true}),
            "audio_tracks.repair" => json!({"folder": "x"}),
            "item.remove" | "item.duplicate" => json!({"item": 1}),
            "item.move" => json!({"item": 1, "to": "up"}),
            "item.transform" => json!({"item": 1, "transform": {}}),
            "item.visible" => json!({"item": 1, "visible": true}),
            "item.lock" => json!({"item": 1, "locked": true}),
            "filter.add" => json!({"source": 1, "kind": "gain"}),
            "filter.remove" => json!({"source": 1, "filter": 1}),
            "filter.settings" => json!({"source": 1, "filter": 1, "settings": {}}),
            "filter.enable" => json!({"source": 1, "filter": 1, "enabled": true}),
            "transition.set" => json!({"kind": "fade"}),
            "transition.duration" => json!({"ms": 100}),
            "transition.settings" => json!({"settings": {}}),
            "app_audio.route" => json!({"app": "player.exe", "routed": true}),
            _ => json!({}),
        };
        let c = Command::from_parts(s.id, &params).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(c.id(), s.id);
        let _ = (SceneRef::Id(SceneId(1)), SourceRef::Id(SourceId(1)), ItemRef::Id(ItemId(1)), Move::Up, C::Undo {});
    }
}
