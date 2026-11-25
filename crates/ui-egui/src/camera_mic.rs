//! A camera source's menu › Add the Camera's Microphone: the audio input that is the camera's own
//! microphone is found off the window's thread, then added to the scene being edited.

use std::sync::Arc;

use serde_json::json;
use studiocraft_model::{SceneCollection, SceneId, SourceId};
use studiocraft_platform::audio::DeviceInfo;

use crate::forms::{Reply, try_read_off};
use crate::i18n::Msg;

/// The kind of the camera source.
pub(crate) const CAMERA: &str = "camera";

/// The kind of the source that captures an audio input.
const AUDIO_INPUT: &str = "audio_input";

/// Finds the microphone of the camera stored in a camera source's settings as the given key.
pub(crate) type Finder = Arc<dyn Fn(&str) -> Option<DeviceInfo> + Send + Sync>;

/// The system's microphones.
pub(crate) fn system() -> Finder {
    Arc::new(studiocraft_platform::camera_audio::microphone_for)
}

/// What a search was started for: the camera source, the camera stored in its settings and the
/// scene that was being edited.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Asked {
    pub camera: SourceId,
    pub key: String,
    pub scene: SceneId,
}

impl Asked {
    /// The search for the camera source `camera` in `c`, with `scene` being edited.
    pub(crate) fn new(c: &SceneCollection, camera: SourceId, scene: SceneId) -> Asked {
        let key = c.source(camera).and_then(|s| s.settings.str("device")).unwrap_or_default().to_string();
        Asked { camera, key, scene }
    }
}

/// A search for one camera source's microphone, running on a thread of its own.
pub(crate) struct Search {
    asked: Asked,
    reply: Arc<Reply<Option<DeviceInfo>>>,
}

impl Search {
    /// Start looking for the microphone of the camera `asked` names. `None` when no thread can be
    /// started for it; the search never runs on the calling thread.
    pub(crate) fn start(find: &Finder, asked: Asked) -> Option<Search> {
        let (find, key) = (Arc::clone(find), asked.key.clone());
        try_read_off(Arc::new(move || find(&key))).map(|reply| Search { asked, reply })
    }

    /// What the search was for and what it found, once it has ended. A search that stopped
    /// without an answer counts as finding nothing.
    pub(crate) fn ended(&self) -> Option<(Asked, Option<DeviceInfo>)> {
        self.reply.ended().map(|found| (self.asked.clone(), found.flatten()))
    }
}

/// What the window does with the microphone found for a camera source.
#[derive(Debug, PartialEq)]
pub(crate) enum Then {
    /// Run a command: add the microphone's source to the scene.
    Run(&'static str, serde_json::Value),
    /// Tell the user why nothing was added; true for an error.
    Say(Msg, bool),
}

/// The longest name taken from a device, leaving room for the number that makes a source name
/// unique within the engine's limit of 256 characters.
const MAX_DEVICE_NAME: usize = 240;

/// A source name from the device name `device`: without control characters, trimmed and cut to
/// [`MAX_DEVICE_NAME`] characters, or "Microphone" in the interface language when nothing is left.
fn source_name(device: &str) -> String {
    let clean: String = device.chars().filter(|c| !c.is_control()).collect();
    let cut: String = clean.trim().chars().take(MAX_DEVICE_NAME).collect();
    match cut.trim_end() {
        "" => tl!("Microphone").to_string(),
        name => name.to_string(),
    }
}

/// What to do once the microphone was looked for as `asked`. The microphone goes into the scene
/// that was being edited when the search started. A source that already captures the microphone
/// is placed in the scene instead of a second one. `None` when, during the search, the camera
/// source or the scene was removed, the camera was taken out of that scene, or the source was
/// set to another camera.
pub(crate) fn then(c: &SceneCollection, asked: &Asked, found: Option<&DeviceInfo>) -> Option<Then> {
    let cam = c.source(asked.camera)?;
    if cam.settings.str("device").unwrap_or_default() != asked.key {
        return None;
    }
    let scene = c.scene(asked.scene)?;
    if !scene.items.iter().any(|i| i.source == asked.camera) {
        return None;
    }
    let Some(mic) = found else {
        return Some(Then::Say(tmsg!("No microphone of the camera in {source} was found", source = cam.name), true));
    };
    // Only a source set to the microphone itself captures it. A source of the default input keeps
    // the device that was the default when it opened, which may be another one now.
    let existing: Vec<_> = c.sources.iter().filter(|s| s.kind == AUDIO_INPUT && s.settings.str("device") == Some(mic.id.as_str())).collect();
    if let Some(global) = existing.iter().find(|s| c.global_audio.contains(&s.id)) {
        return Some(Then::Say(tmsg!("{source} already plays in every scene", source = global.name), false));
    }
    if let Some(placed) = existing.iter().find(|s| scene.items.iter().any(|i| i.source == s.id)) {
        return Some(Then::Say(tmsg!("{source} is already in this scene", source = placed.name), false));
    }
    let Some(existing) = existing.first() else {
        let name = c.unique_source_name(&source_name(&mic.name));
        return Some(Then::Run("source.add", json!({"scene": scene.id, "kind": AUDIO_INPUT, "name": name, "settings": {"device": mic.id}})));
    };
    Some(Then::Run("source.add_existing", json!({"scene": scene.id, "source": existing.id})))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::mpsc;

    use studiocraft_model::{Scene, SceneItem, SourceConfig};
    use studiocraft_platform::audio::DEFAULT_DEVICE;

    use super::*;

    fn mic(id: &str, name: &str) -> DeviceInfo {
        DeviceInfo { id: id.into(), name: name.into(), default: false }
    }

    /// Add a source of `kind` named `name` with the `device` setting to `c`.
    fn add_source(c: &mut SceneCollection, kind: &str, name: &str, device: &str) -> SourceId {
        let id = SourceId(c.alloc_id());
        let s: SourceConfig = serde_json::from_value(json!({"id": id, "kind": kind, "name": name, "settings": {"device": device}})).unwrap();
        c.sources.push(s);
        id
    }

    /// Place `source` in `scene`.
    fn place(c: &mut SceneCollection, scene: SceneId, source: SourceId) {
        let item: SceneItem = serde_json::from_value(json!({"id": c.alloc_id(), "source": source})).unwrap();
        c.scene_mut(scene).unwrap().items.push(item);
    }

    /// Add a scene named `name` to `c`.
    fn add_scene(c: &mut SceneCollection, name: &str) -> SceneId {
        let id = SceneId(c.alloc_id());
        let s: Scene = serde_json::from_value(json!({"id": id, "name": name, "items": []})).unwrap();
        c.scenes.push(s);
        id
    }

    /// A collection with a camera source in its first scene, and the search for its microphone
    /// with that scene being edited.
    fn with_camera() -> (SceneCollection, Asked) {
        let mut c = SceneCollection::new("Test", "Scene");
        let scene = c.scenes[0].id;
        let camera = add_source(&mut c, CAMERA, "Desk Camera", "HD Webcam\u{1f}cam-1");
        place(&mut c, scene, camera);
        let asked = Asked::new(&c, camera, scene);
        assert_eq!(asked, Asked { camera, key: "HD Webcam\u{1f}cam-1".into(), scene });
        (c, asked)
    }

    #[test]
    fn a_found_microphone_is_added_as_a_new_source_named_after_it() {
        let (mut c, asked) = with_camera();
        add_source(&mut c, "text", "Microphone (HD Webcam)", "");
        let found = mic("Microphone (HD Webcam)", "Microphone (HD Webcam)");
        let Some(Then::Run(id, params)) = then(&c, &asked, Some(&found)) else { panic!("a command") };
        assert_eq!(id, "source.add");
        assert_eq!(params["kind"], AUDIO_INPUT);
        assert_eq!(params["scene"], json!(asked.scene));
        assert_eq!(params["settings"]["device"], "Microphone (HD Webcam)");
        // The name is the microphone's, made unique among the sources.
        assert_eq!(params["name"], c.unique_source_name("Microphone (HD Webcam)"));
        assert_ne!(params["name"], "Microphone (HD Webcam)");
    }

    #[test]
    fn a_device_name_becomes_a_valid_source_name() {
        assert_eq!(source_name("  Microphone (HD Webcam)\n"), "Microphone (HD Webcam)");
        assert_eq!(source_name("Mic\u{7}rophone"), "Microphone");
        assert_eq!(source_name(" \t\u{0} "), "Microphone");
        let long = "x".repeat(1_000);
        assert_eq!(source_name(&long).chars().count(), MAX_DEVICE_NAME);
        // A long name that is taken still gets a unique name within the engine's limit.
        let (mut c, asked) = with_camera();
        add_source(&mut c, "text", &"x".repeat(MAX_DEVICE_NAME), "");
        let Some(Then::Run(_, params)) = then(&c, &asked, Some(&mic("mic-1", &long))) else { panic!("a command") };
        let name = params["name"].as_str().unwrap();
        assert!(name.chars().count() <= 256 && name.ends_with(" 2"), "{name}");
    }

    #[test]
    fn a_device_without_a_name_gets_the_fallback_in_the_interface_language() {
        use craft_i18n::Lang;

        use crate::i18n::set_lang;
        set_lang(Lang::PSEUDO);
        let name = source_name(" \t\u{0} ");
        set_lang(Lang::EN);
        assert_eq!(name, "\u{27e6}Microphone\u{27e7}");
        assert_eq!(source_name(""), "Microphone");
    }

    #[test]
    fn a_source_that_already_captures_the_microphone_is_placed_instead() {
        let (mut c, asked) = with_camera();
        let other = add_scene(&mut c, "Other");
        let existing = add_source(&mut c, AUDIO_INPUT, "Webcam Mic", "mic-1");
        place(&mut c, other, existing);
        let found = mic("mic-1", "Microphone (HD Webcam)");
        let placed = Some(Then::Run("source.add_existing", json!({"scene": asked.scene, "source": existing})));
        assert_eq!(then(&c, &asked, Some(&found)), placed);
        // Already in the scene: nothing is added.
        place(&mut c, asked.scene, existing);
        let Some(Then::Say(m, false)) = then(&c, &asked, Some(&found)) else { panic!("a message") };
        assert_eq!(m.english(), "Webcam Mic is already in this scene");
    }

    #[test]
    fn a_microphone_that_plays_in_every_scene_is_not_added_again() {
        let (mut c, asked) = with_camera();
        let existing = add_source(&mut c, AUDIO_INPUT, "Mic/Aux", "mic-1");
        c.global_audio.push(existing);
        let Some(Then::Say(m, false)) = then(&c, &asked, Some(&mic("mic-1", "Webcam"))) else { panic!("a message") };
        assert_eq!(m.english(), "Mic/Aux already plays in every scene");
    }

    #[test]
    fn no_microphone_found_is_reported_and_a_removed_camera_does_nothing() {
        let (c, asked) = with_camera();
        let Some(Then::Say(m, true)) = then(&c, &asked, None) else { panic!("an error") };
        assert_eq!(m.english(), "No microphone of the camera in Desk Camera was found");
        let removed = Asked { camera: SourceId(9_999), ..asked };
        assert_eq!(then(&c, &removed, Some(&mic("mic-1", "Webcam"))), None);
    }

    #[test]
    fn the_microphone_goes_to_the_scene_edited_when_the_search_started() {
        let (mut c, asked) = with_camera();
        // The user moved to another scene during the search.
        add_scene(&mut c, "Other");
        let Some(Then::Run(_, params)) = then(&c, &asked, Some(&mic("mic-1", "Webcam"))) else { panic!("a command") };
        assert_eq!(params["scene"], json!(asked.scene));
        // That scene was removed during the search: nothing is added.
        c.scenes.retain(|s| s.id != asked.scene);
        assert_eq!(then(&c, &asked, Some(&mic("mic-1", "Webcam"))), None);
    }

    #[test]
    fn a_camera_taken_out_of_the_scene_during_the_search_ignores_the_answer() {
        let (mut c, asked) = with_camera();
        let other = add_scene(&mut c, "Other");
        place(&mut c, other, asked.camera);
        c.scene_mut(asked.scene).unwrap().items.retain(|i| i.source != asked.camera);
        assert_eq!(then(&c, &asked, Some(&mic("mic-1", "Webcam"))), None);
    }

    #[test]
    fn any_source_of_the_microphone_that_plays_in_every_scene_stops_the_add() {
        let (mut c, asked) = with_camera();
        add_source(&mut c, AUDIO_INPUT, "Webcam Mic", "mic-1");
        let global = add_source(&mut c, AUDIO_INPUT, "Mic/Aux", "mic-1");
        c.global_audio.push(global);
        let Some(Then::Say(m, false)) = then(&c, &asked, Some(&mic("mic-1", "Webcam"))) else { panic!("a message") };
        assert_eq!(m.english(), "Mic/Aux already plays in every scene");
        // The same holds for one that is already in the scene.
        c.global_audio.clear();
        place(&mut c, asked.scene, global);
        let Some(Then::Say(m, false)) = then(&c, &asked, Some(&mic("mic-1", "Webcam"))) else { panic!("a message") };
        assert_eq!(m.english(), "Mic/Aux is already in this scene");
    }

    #[test]
    fn a_source_of_the_default_input_does_not_count_as_capturing_the_microphone() {
        let (mut c, asked) = with_camera();
        // The default input's source opened whichever device was the default then, so it is not
        // taken as a capture of the microphone, even when the microphone is the default now.
        let global = add_source(&mut c, AUDIO_INPUT, "Microphone", DEFAULT_DEVICE);
        c.global_audio.push(global);
        let placed = add_source(&mut c, AUDIO_INPUT, "Mic", "");
        place(&mut c, asked.scene, placed);
        let default = DeviceInfo { default: true, ..mic("mic-1", "Webcam") };
        let Some(Then::Run(id, params)) = then(&c, &asked, Some(&default)) else { panic!("a command") };
        assert_eq!(id, "source.add");
        assert_eq!(params["settings"]["device"], "mic-1");
        let Some(Then::Run(id, _)) = then(&c, &asked, Some(&mic("mic-1", "Webcam"))) else { panic!("a command") };
        assert_eq!(id, "source.add");
    }

    #[test]
    fn a_camera_source_set_to_another_camera_during_the_search_ignores_the_answer() {
        let (mut c, asked) = with_camera();
        let cam = c.source_mut(asked.camera).unwrap();
        cam.settings.set("device", "Other Cam\u{1f}cam-2");
        assert_eq!(then(&c, &asked, Some(&mic("mic-1", "Webcam"))), None);
        assert_eq!(then(&c, &asked, None), None);
    }

    #[test]
    fn the_search_runs_off_the_calling_thread_and_is_taken_once_it_ends() {
        let (go, wait) = mpsc::sync_channel::<()>(0);
        let wait = Mutex::new(wait);
        let asked = Arc::new(Mutex::new(String::new()));
        let seen = Arc::clone(&asked);
        let find: Finder = Arc::new(move |key: &str| {
            *seen.lock().unwrap() = key.to_string();
            wait.lock().unwrap().recv().ok()?;
            Some(DeviceInfo { id: "mic-1".into(), name: "Webcam Mic".into(), default: false })
        });
        let want = Asked { camera: SourceId(4), key: "HD Webcam\u{1f}cam-1".into(), scene: SceneId(1) };
        let search = Search::start(&find, want.clone()).expect("a thread starts");
        // The finder is still waiting: the caller carries on.
        assert_eq!(search.ended(), None);
        go.send(()).unwrap();
        let ended = loop {
            if let Some(e) = search.ended() {
                break e;
            }
            std::thread::yield_now();
        };
        assert_eq!(ended, (want, Some(mic("mic-1", "Webcam Mic"))));
        assert_eq!(*asked.lock().unwrap(), "HD Webcam\u{1f}cam-1");
    }
}
