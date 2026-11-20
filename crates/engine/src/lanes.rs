//! The lanes of the Application Audio panel: one for each microphone source and one for each
//! running application with audio, with whether the program hears it.

use std::collections::HashSet;

use craft_app_audio::{AppInfo, AppKey, AppList};
use serde::Serialize;
use studiocraft_model::{SceneCollection, SourceConfig, SourceId};

/// The plug-in id of the source that captures one application, which `app_audio.route` adds.
pub const APP_AUDIO_KIND: &str = "app_audio";
/// The plug-in id of the microphone source.
pub const MIC_KIND: &str = "audio_input";

/// What a lane carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LaneKind {
    Microphone,
    Application,
}

/// One lane.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Lane {
    pub kind: LaneKind,
    /// The name to show: the source's name for a microphone, the program's name for an
    /// application (with its folder when another listed program has the same file name).
    pub name: String,
    /// An application lane's `app` value, as `app_audio.route` and the `app_audio` source take it.
    pub app: Option<String>,
    /// The source that carries the lane's audio. An application lane has none until it is routed.
    pub source: Option<SourceId>,
    /// Whether the lane's source is a global audio source, which every scene hears. This is what
    /// `app_audio.route` sets.
    pub routed: bool,
    /// Whether the program hears the lane: its source is routed or shown in the program scene,
    /// and its monitoring does not leave it out of the program.
    pub heard: bool,
    /// The source's volume, 0 to 1; 1 for a lane without a source.
    pub gain: f32,
    pub muted: bool,
    /// An application's level as the system reports it, 0 to 1, read without capturing it.
    /// `None` without a current reading, and for a microphone.
    pub peak: Option<f32>,
    /// Whether an application lane's application is running. A routed application that quit
    /// keeps its lane, with `running` false.
    pub running: bool,
    /// Whether one of the application's audio streams is running (it may be paused or silent).
    pub playing: bool,
    /// The `app` value of another listed application whose capture already records this one's
    /// processes, so routing both hears this application twice.
    pub within: Option<String>,
}

/// Every lane, microphones first, then applications by name, then routed applications that are
/// not running.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Lanes {
    /// Whether this system captures one application's audio.
    pub supported: bool,
    /// Why not, when it doesn't. English.
    pub reason: Option<String>,
    /// The applications have not been listed yet; the lanes hold the last list read, if any.
    pub listing: bool,
    /// Why the applications could not be listed. English.
    pub error: Option<String>,
    pub lanes: Vec<Lane>,
}

/// The application an `app_audio` source's `app` setting names.
pub fn source_key(s: &SourceConfig) -> Option<AppKey> {
    (s.kind == APP_AUDIO_KIND).then(|| s.settings.str("app").and_then(AppKey::decode)).flatten()
}

/// Whether two keys name the same application: the same key, the same listed application, or
/// for an application that is not listed, the same file name with no path on either side that
/// tells them apart. The list is read as a capture source reads it, so a source saved for a
/// program that has since moved matches the program at its new path.
pub fn same_app(a: &AppKey, b: &AppKey, list: &AppList) -> bool {
    if a == b {
        return true;
    }
    match (list.find(a), list.find(b)) {
        (Some(x), Some(y)) => std::ptr::eq(x, y),
        (None, None) => a.name == b.name && (a.path.is_none() || b.path.is_none()),
        _ => false,
    }
}

/// The sources the program hears: the global audio sources and the visible items of the program
/// scene, leaving out the sources whose monitoring keeps them out of the program.
pub fn heard(c: &SceneCollection) -> HashSet<SourceId> {
    let shown = c.scene(c.program_scene).into_iter().flat_map(|s| s.items.iter().filter(|i| i.visible).map(|i| i.source));
    let in_program = |id: &SourceId| c.source(*id).is_some_and(|s| s.audio.monitor.in_program());
    c.global_audio.iter().copied().chain(shown).filter(in_program).collect()
}

fn lane(kind: LaneKind, name: String, source: Option<&SourceConfig>, c: &SceneCollection, heard: &HashSet<SourceId>) -> Lane {
    Lane {
        kind,
        name,
        app: None,
        source: source.map(|s| s.id),
        routed: source.is_some_and(|s| c.global_audio.contains(&s.id)),
        heard: source.is_some_and(|s| heard.contains(&s.id)),
        gain: source.map_or(1.0, |s| s.audio.gain),
        muted: source.is_some_and(|s| s.audio.muted),
        peak: None,
        running: false,
        playing: false,
        within: None,
    }
}

/// The lanes of collection `c` with the applications in `list`; `peak` reads an application's
/// meter.
pub fn lanes(c: &SceneCollection, list: &AppList, peak: impl Fn(&AppInfo) -> Option<f32>) -> Vec<Lane> {
    let heard = heard(c);
    let global = |s: &SourceConfig| c.global_audio.contains(&s.id);
    let mut out: Vec<Lane> =
        c.sources.iter().filter(|s| s.kind == MIC_KIND).map(|s| lane(LaneKind::Microphone, s.name.clone(), Some(s), c, &heard)).collect();
    let apps: Vec<(&SourceConfig, AppKey)> = c.sources.iter().filter_map(|s| source_key(s).map(|k| (s, k))).collect();
    let mut used = vec![false; apps.len()];
    for a in &list.apps {
        // A source the program hears comes first, then a routed one, so the lane's controls act on
        // the source the program hears.
        let mut matching: Vec<usize> = (0..apps.len()).filter(|i| apps.get(*i).is_some_and(|(_, k)| same_app(k, &a.key, list))).collect();
        matching.sort_by_key(|i| apps.get(*i).map_or((true, true), |(s, _)| (!heard.contains(&s.id), !global(s))));
        for i in &matching {
            if let Some(u) = used.get_mut(*i) {
                *u = true;
            }
        }
        let source = matching.first().and_then(|i| apps.get(*i)).map(|(s, _)| *s);
        let shared = list.apps.iter().filter(|b| b.key.name == a.key.name).nth(1).is_some();
        let name = if shared && !a.path.is_empty() { format!("{} ({})", a.name, a.path) } else { a.name.clone() };
        let mut l = lane(LaneKind::Application, name, source, c, &heard);
        l.routed = matching.iter().any(|i| apps.get(*i).is_some_and(|(s, _)| global(s)));
        l.heard = matching.iter().any(|i| apps.get(*i).is_some_and(|(s, _)| heard.contains(&s.id)));
        l.app = Some(source.and_then(|s| s.settings.str("app")).map_or_else(|| a.choice(), str::to_string));
        l.peak = peak(a);
        l.running = true;
        l.playing = a.playing;
        l.within = a.within.clone();
        out.push(l);
    }
    // Applications that are not running keep a lane while a source for them is routed, so they
    // can be unrouted. Two sources for one application share a lane.
    let mut gone: Vec<(&SourceConfig, &AppKey)> = Vec::new();
    for ((s, k), u) in apps.iter().zip(&used) {
        if *u || !global(s) || gone.iter().any(|(_, g)| same_app(g, k, list)) {
            continue;
        }
        gone.push((s, k));
    }
    for (s, _) in gone {
        let mut l = lane(LaneKind::Application, s.name.clone(), Some(s), c, &heard);
        l.app = s.settings.str("app").map(str::to_string);
        out.push(l);
    }
    out
}
