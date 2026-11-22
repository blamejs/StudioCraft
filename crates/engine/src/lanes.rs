//! The lanes of the Application Audio panel: one for each microphone source and one for each
//! running application with audio, with whether the program hears it.

use std::collections::HashSet;

/// The key an application lane's `app` value encodes, for [`crate::Engine::app_audio_peak`].
pub use craft_app_audio::AppKey;
use craft_app_audio::{AppInfo, AppList};
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
    /// `None` for a microphone, and for the lane of a second source the program hears for an
    /// application that already has a lane.
    pub app: Option<String>,
    /// The source that carries the lane's audio. An application lane has none until it is routed.
    pub source: Option<SourceId>,
    /// Whether the lane's source is a global audio source, which every scene hears. This is what
    /// `app_audio.route` sets.
    pub routed: bool,
    /// Whether a global audio source of the lane reaches the program: one whose monitoring does
    /// not leave it out of the program.
    pub routed_heard: bool,
    /// Whether the program hears the lane: its source is routed or shown in the program scene,
    /// and its monitoring does not leave it out of the program.
    pub heard: bool,
    /// The source's volume as a linear gain, 0 to 10 (1 is 0 dB, 10 is +20 dB); 1 for a lane
    /// without a source.
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
    /// Whether the program hears a desktop audio source that is not muted. A desktop audio
    /// capture also records what a routed application plays on that device.
    pub desktop_heard: bool,
}

/// The kind of a desktop audio source, which captures what plays on an output device.
pub const DESKTOP_KIND: &str = "audio_output";

/// Whether the program hears a desktop audio source of `c` that is not muted.
pub fn desktop_heard(c: &SceneCollection) -> bool {
    let heard = heard(c);
    c.sources.iter().any(|s| s.kind == DESKTOP_KIND && !s.audio.muted && heard.contains(&s.id))
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

/// Whether sources of collection `c` name more than one program path with file name `name`. A
/// source keyed by that file name alone then stands for neither path.
pub fn shared_name(c: &SceneCollection, name: &str) -> bool {
    let mut paths = c.sources.iter().filter_map(source_key).filter(|k| k.name == name).filter_map(|k| k.path);
    let first = paths.next();
    paths.any(|p| Some(&p) != first.as_ref())
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
        routed_heard: source.is_some_and(|s| c.global_audio.contains(&s.id) && heard.contains(&s.id)),
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
        l.routed_heard = matching.iter().any(|i| apps.get(*i).is_some_and(|(s, _)| global(s) && heard.contains(&s.id)));
        l.heard = matching.iter().any(|i| apps.get(*i).is_some_and(|(s, _)| heard.contains(&s.id)));
        l.app = Some(source.and_then(|s| s.settings.str("app")).map_or_else(|| a.choice(), str::to_string));
        l.peak = peak(a);
        l.running = true;
        l.playing = a.playing;
        l.within = a.within.clone();
        out.push(l);
        let extra = matching.iter().skip(1).filter_map(|i| apps.get(*i)).filter(|(s, _)| heard.contains(&s.id));
        for (s, _) in extra {
            let mut l = lane(LaneKind::Application, s.name.clone(), Some(s), c, &heard);
            l.running = true;
            l.playing = a.playing;
            l.within = a.within.clone();
            l.peak = peak(a);
            out.push(l);
        }
    }
    // Applications that are not running keep a lane while a source for them is routed, so they
    // can be unrouted. Sources for one program path share a lane, a source the program hears
    // first. A source keyed by file name alone joins the path with that file name when the
    // collection names only one, and otherwise has a lane of its own, as `app_audio.route`
    // matches it.
    let mut gone: Vec<(&AppKey, Vec<&SourceConfig>)> = Vec::new();
    let unused = || apps.iter().zip(&used).filter(|((s, _), u)| !**u && global(s)).map(|(a, _)| a);
    for (s, k) in unused().filter(|(_, k)| k.path.is_some()) {
        match gone.iter_mut().find(|(g, _)| *g == k) {
            Some((_, g)) => g.push(s),
            None => gone.push((k, vec![s])),
        }
    }
    for (s, k) in unused().filter(|(_, k)| k.path.is_none()) {
        let path = (!shared_name(c, &k.name)).then(|| gone.iter().position(|(g, _)| g.path.is_some() && g.name == k.name)).flatten();
        let at = path.or_else(|| gone.iter().position(|(g, _)| *g == k));
        match at.and_then(|i| gone.get_mut(i)) {
            Some((_, g)) => g.push(s),
            None => gone.push((k, vec![s])),
        }
    }
    for (key, mut group) in gone {
        group.sort_by_key(|s| !heard.contains(&s.id));
        let Some(first) = group.first() else { continue };
        let mut l = lane(LaneKind::Application, first.name.clone(), Some(first), c, &heard);
        // The route key is the group's own key, so routing it acts on every source of the lane.
        l.app = group.iter().find(|s| source_key(s).as_ref() == Some(key)).and_then(|s| s.settings.str("app")).map(str::to_string);
        out.push(l);
        for s in group.iter().skip(1).filter(|s| heard.contains(&s.id)) {
            out.push(lane(LaneKind::Application, s.name.clone(), Some(s), c, &heard));
        }
    }
    out
}
