//! Source threads. Each source of the scene collection runs on a thread of its own: the thread
//! creates the plug-in instance, calls it at its pace, and applies new settings sent to it. A
//! source that fails to start is retried every few seconds and its error is shown; a plug-in
//! that panics stops only its own thread.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use studiocraft_media::clock::MasterClock;
use studiocraft_model::{SceneCollection, Settings, SourceId};
use studiocraft_plugin::{Pace, Registry, Source, SourceInstance, SourceIo};

/// How long a source that failed to start waits before trying again.
pub const RETRY: Duration = Duration::from_secs(3);

enum Msg {
    Update(Settings),
    Stop,
}

/// A running source thread.
pub struct SourceHandle {
    pub io: SourceIo,
    ctl: Sender<Msg>,
    error: Arc<Mutex<Option<String>>>,
    notice: Arc<Mutex<Option<&'static str>>>,
}

impl SourceHandle {
    /// Start `plugin` with `settings` on a new thread.
    pub fn spawn(id: SourceId, plugin: Arc<dyn Source>, settings: Settings, clock: MasterClock) -> std::io::Result<SourceHandle> {
        let io = SourceIo::new(clock);
        let (ctl, rx) = channel();
        let (error, notice) = (Arc::new(Mutex::new(None)), Arc::new(Mutex::new(None)));
        let status = Status { error: Arc::clone(&error), notice: Arc::clone(&notice) };
        let thread_io = io.clone();
        std::thread::Builder::new()
            .name(format!("source {id} ({})", plugin.id()))
            .spawn(move || run(plugin.as_ref(), settings, &thread_io, &rx, &status))?;
        Ok(SourceHandle { io, ctl, error, notice })
    }

    /// New settings for the running instance.
    pub fn update(&self, settings: Settings) {
        // A thread that has ended has nothing to update.
        let _ = self.ctl.send(Msg::Update(settings));
    }

    /// The last error, if the source is not working. Never waits: a busy lock reads as no news.
    pub fn error(&self) -> Option<String> {
        self.error.try_lock().ok().and_then(|e| e.clone())
    }

    /// The note a working source shows ([`SourceInstance::notice`]). Never waits: a busy lock
    /// reads as no news.
    pub fn notice(&self) -> Option<&'static str> {
        self.notice.try_lock().ok().and_then(|n| *n)
    }
}

impl Drop for SourceHandle {
    fn drop(&mut self) {
        // The thread ends at its next wake-up; nothing waits for it.
        let _ = self.ctl.send(Msg::Stop);
    }
}

/// What a source thread reports to its [`SourceHandle`].
struct Status {
    error: Arc<Mutex<Option<String>>>,
    notice: Arc<Mutex<Option<&'static str>>>,
}

impl Status {
    /// The outcome of a call into the instance: its note when it works, or the error.
    fn set(&self, r: Result<Option<&'static str>, String>) {
        let (notice, error) = match r {
            Ok(n) => (n, None),
            Err(e) => (None, Some(e)),
        };
        if let Ok(mut s) = self.error.lock() {
            *s = error;
        }
        if let Ok(mut n) = self.notice.lock() {
            *n = notice;
        }
    }
}

/// Run `f`, turning a panic into an error message.
fn guarded<T>(what: &str, f: impl FnOnce() -> Result<T, studiocraft_plugin::PluginError>) -> Result<T, String> {
    match std::panic::catch_unwind(AssertUnwindSafe(f)) {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(e.to_string()),
        Err(_) => Err(format!("internal error in {what}")),
    }
}

fn run(plugin: &dyn Source, mut settings: Settings, io: &SourceIo, rx: &Receiver<Msg>, status: &Status) {
    let mut instance: Option<Box<dyn SourceInstance>> = None;
    let mut retry_at = Instant::now();
    let mut next = Instant::now();
    // Whether the last `produce` of a running instance failed; a static source tries again.
    let mut failed = false;
    loop {
        if instance.is_none() && Instant::now() >= retry_at {
            match guarded("source start", || plugin.create(&settings, io)) {
                Ok(mut i) => {
                    // The note is read inside the guard too: a plug-in can panic in it.
                    let r = guarded("source", || i.produce(io).map(|()| i.notice()));
                    failed = r.is_err();
                    retry_at = Instant::now() + RETRY;
                    status.set(r);
                    instance = Some(i);
                    next = Instant::now();
                }
                Err(e) => {
                    status.set(Err(e));
                    retry_at = Instant::now() + RETRY;
                }
            }
        }
        let wait = match instance.as_ref().map(|i| i.pace()) {
            Some(Pace::Every(period)) => {
                let now = Instant::now();
                if now >= next {
                    if let Some(i) = instance.as_mut() {
                        let r = guarded("source", || i.produce(io).map(|()| i.notice()));
                        if let Err(e) = &r {
                            log::debug!("source produce failed: {e}");
                        }
                        status.set(r);
                    }
                    // Late by more than a period: start counting again from now.
                    next = if now.duration_since(next) > period { now + period } else { next + period };
                }
                next.saturating_duration_since(Instant::now())
            }
            Some(Pace::Static) if failed => {
                if Instant::now() >= retry_at
                    && let Some(i) = instance.as_mut()
                {
                    let r = guarded("source", || i.produce(io).map(|()| i.notice()));
                    failed = r.is_err();
                    retry_at = Instant::now() + RETRY;
                    status.set(r);
                }
                if failed { retry_at.saturating_duration_since(Instant::now()) } else { Duration::from_millis(500) }
            }
            Some(Pace::Static) => Duration::from_millis(500),
            None => retry_at.saturating_duration_since(Instant::now()).max(Duration::from_millis(50)),
        };
        match rx.recv_timeout(wait) {
            Ok(Msg::Update(s)) => {
                settings = s;
                if let Some(i) = instance.as_mut() {
                    let r = guarded("source settings", || i.update(&settings, io).and_then(|()| i.produce(io)).map(|()| i.notice()));
                    failed = r.is_err();
                    retry_at = Instant::now() + RETRY;
                    status.set(r);
                } else {
                    retry_at = Instant::now();
                }
            }
            Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

struct Entry {
    kind: String,
    settings: Settings,
    handle: SourceHandle,
}

/// The running sources of a scene collection, kept in step with it by [`SourceSet::sync`].
pub struct SourceSet {
    clock: MasterClock,
    running: HashMap<SourceId, Entry>,
}

impl SourceSet {
    pub fn new(clock: MasterClock) -> Self {
        Self { clock, running: HashMap::new() }
    }

    /// Start the sources `c` has that are not running, restart those whose kind changed, send
    /// new settings, and stop the ones `c` no longer has. Returns the sources that could not
    /// start, with the reason.
    pub fn sync(&mut self, c: &SceneCollection, registry: &Registry) -> Vec<(SourceId, String)> {
        let mut problems = Vec::new();
        self.running.retain(|id, e| c.source(*id).is_some_and(|s| s.kind == e.kind));
        for s in &c.sources {
            match self.running.get_mut(&s.id) {
                Some(e) => {
                    if e.settings != s.settings {
                        e.settings = s.settings.clone();
                        e.handle.update(s.settings.clone());
                    }
                }
                None => {
                    let plugin = match registry.source(&s.kind) {
                        Ok(p) => p,
                        Err(e) => {
                            problems.push((s.id, e.to_string()));
                            continue;
                        }
                    };
                    let settings =
                        if s.kind_version < plugin.version() { plugin.migrate(s.kind_version, s.settings.clone()) } else { s.settings.clone() };
                    match SourceHandle::spawn(s.id, plugin, settings, self.clock) {
                        Ok(handle) => {
                            self.running.insert(s.id, Entry { kind: s.kind.clone(), settings: s.settings.clone(), handle });
                        }
                        Err(e) => problems.push((s.id, format!("no thread for the source: {e}"))),
                    }
                }
            }
        }
        problems
    }

    pub fn io(&self, id: SourceId) -> Option<&SourceIo> {
        self.running.get(&id).map(|e| &e.handle.io)
    }

    pub fn error(&self, id: SourceId) -> Option<String> {
        self.running.get(&id).and_then(|e| e.handle.error())
    }

    /// The note a working source shows ([`SourceHandle::notice`]).
    pub fn notice(&self, id: SourceId) -> Option<&'static str> {
        self.running.get(&id).and_then(|e| e.handle.notice())
    }

    /// The size of what a source shows, once it has shown something.
    pub fn size(&self, id: SourceId) -> Option<(u32, u32)> {
        self.io(id).and_then(|io| io.video.size())
    }

    /// Whether a running source has published its first video content, a picture or nothing.
    pub fn published(&self, id: SourceId) -> bool {
        self.io(id).is_some_and(|io| io.video.published())
    }

    pub fn len(&self) -> usize {
        self.running.len()
    }

    pub fn is_empty(&self) -> bool {
        self.running.is_empty()
    }

    pub fn clock(&self) -> MasterClock {
        self.clock
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use studiocraft_model::{AudioState, SceneCollection, SettingsSchema, SourceConfig};
    use studiocraft_plugin::{Plugin, PluginError, SourceCaps, VideoContent};

    use super::*;

    static CREATED: AtomicU32 = AtomicU32::new(0);

    struct Solid;
    impl Plugin for Solid {
        fn id(&self) -> &'static str {
            "solid"
        }
        fn name(&self) -> &'static str {
            "Solid"
        }
        fn schema(&self) -> SettingsSchema {
            SettingsSchema::default()
        }
    }
    struct SolidInstance(u32);
    impl SourceInstance for SolidInstance {
        fn pace(&self) -> Pace {
            Pace::Static
        }
        fn produce(&mut self, io: &SourceIo) -> Result<(), PluginError> {
            io.video.publish(VideoContent::Solid { rgba: [1, 2, 3, 4], width: self.0, height: 10 });
            Ok(())
        }
        fn update(&mut self, s: &Settings, _: &SourceIo) -> Result<(), PluginError> {
            self.0 = u32::try_from(s.i64("w").unwrap_or(10)).unwrap_or(10);
            Ok(())
        }
    }
    impl Source for Solid {
        fn caps(&self) -> SourceCaps {
            SourceCaps { video: true, audio: false }
        }
        fn create(&self, _: &Settings, _: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
            CREATED.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(SolidInstance(10)))
        }
    }

    struct Panicky;
    impl Plugin for Panicky {
        fn id(&self) -> &'static str {
            "panicky"
        }
        fn name(&self) -> &'static str {
            "Panicky"
        }
        fn schema(&self) -> SettingsSchema {
            SettingsSchema::default()
        }
    }
    impl Source for Panicky {
        fn caps(&self) -> SourceCaps {
            SourceCaps::default()
        }
        fn create(&self, _: &Settings, _: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
            panic!("plug-in bug")
        }
    }

    /// A source that works and has a note to show, or panics when asked for it.
    struct Noting(bool);
    impl Plugin for Noting {
        fn id(&self) -> &'static str {
            "noting"
        }
        fn name(&self) -> &'static str {
            "Noting"
        }
        fn schema(&self) -> SettingsSchema {
            SettingsSchema::default()
        }
    }
    struct NotingInstance(bool);
    impl SourceInstance for NotingInstance {
        fn pace(&self) -> Pace {
            Pace::Static
        }
        fn produce(&mut self, _: &SourceIo) -> Result<(), PluginError> {
            Ok(())
        }
        fn update(&mut self, _: &Settings, _: &SourceIo) -> Result<(), PluginError> {
            Ok(())
        }
        fn notice(&self) -> Option<&'static str> {
            if self.0 {
                panic!("plug-in bug");
            }
            Some("the setting is not followed")
        }
    }
    impl Source for Noting {
        fn caps(&self) -> SourceCaps {
            SourceCaps { video: true, audio: false }
        }
        fn create(&self, _: &Settings, _: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
            Ok(Box::new(NotingInstance(self.0)))
        }
    }

    /// Run `plugin`'s thread until it has started the source and produced once.
    fn started(plugin: &Noting) -> Status {
        let (ctl, rx) = channel();
        ctl.send(Msg::Stop).unwrap();
        let status = Status { error: Arc::default(), notice: Arc::default() };
        run(plugin, Settings::new(), &SourceIo::new(MasterClock::new()), &rx, &status);
        status
    }

    #[test]
    fn a_working_sources_note_is_reported_apart_from_its_error() {
        let status = started(&Noting(false));
        assert_eq!(*status.notice.lock().unwrap(), Some("the setting is not followed"));
        assert_eq!(*status.error.lock().unwrap(), None, "the source is working");
        let status = started(&Noting(true));
        assert_eq!(*status.error.lock().unwrap(), Some("internal error in source".into()), "a panic in the note becomes an error");
        assert_eq!(*status.notice.lock().unwrap(), None);
    }

    fn wait_for(mut f: impl FnMut() -> bool) -> bool {
        (0..200).any(|_| {
            std::thread::sleep(Duration::from_millis(10));
            f()
        })
    }

    #[test]
    fn sources_start_update_and_stop_with_the_collection() {
        let mut reg = Registry::new();
        reg.register_source(Arc::new(Solid)).unwrap();
        reg.register_source(Arc::new(Panicky)).unwrap();
        let mut c = SceneCollection::new("t", "Scene");
        let add = |c: &mut SceneCollection, kind: &str| {
            let id = SourceId(c.alloc_id());
            c.sources.push(SourceConfig {
                id,
                kind: kind.into(),
                kind_version: 1,
                name: kind.into(),
                settings: Settings::default(),
                filters: vec![],
                audio: AudioState::default(),
            });
            id
        };
        let a = add(&mut c, "solid");
        let p = add(&mut c, "panicky");
        let missing = add(&mut c, "missing");
        let mut set = SourceSet::new(MasterClock::new());
        let problems = set.sync(&c, &reg);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].0, missing);
        assert!(wait_for(|| set.size(a) == Some((10, 10))));
        assert!(wait_for(|| set.error(p).is_some_and(|e| e.contains("internal error"))), "a panic becomes an error");

        c.source_mut(a).unwrap().settings = Settings::new().with("w", 33);
        set.sync(&c, &reg);
        assert!(wait_for(|| set.size(a) == Some((33, 10))));
        let created = CREATED.load(Ordering::SeqCst);
        set.sync(&c, &reg);
        assert_eq!(CREATED.load(Ordering::SeqCst), created, "unchanged sources are left running");

        c.sources.retain(|s| s.id != a);
        set.sync(&c, &reg);
        assert!(set.io(a).is_none());
    }

    /// A static source whose first `produce` fails, as an image whose file is not there yet.
    struct LateFile(Mutex<Sender<u32>>);
    impl Plugin for LateFile {
        fn id(&self) -> &'static str {
            "late"
        }
        fn name(&self) -> &'static str {
            "Late file"
        }
        fn schema(&self) -> SettingsSchema {
            SettingsSchema::default()
        }
    }
    struct LateInstance(u32, Sender<u32>);
    impl SourceInstance for LateInstance {
        fn pace(&self) -> Pace {
            Pace::Static
        }
        fn produce(&mut self, _: &SourceIo) -> Result<(), PluginError> {
            self.0 += 1;
            let _ = self.1.send(self.0);
            if self.0 == 1 { Err(PluginError::Unavailable("the file was not found".into())) } else { Ok(()) }
        }
        fn update(&mut self, _: &Settings, _: &SourceIo) -> Result<(), PluginError> {
            Ok(())
        }
    }
    impl Source for LateFile {
        fn caps(&self) -> SourceCaps {
            SourceCaps { video: true, audio: false }
        }
        fn create(&self, _: &Settings, _: &SourceIo) -> Result<Box<dyn SourceInstance>, PluginError> {
            let tx = self.0.lock().map_err(|_| PluginError::Unavailable("lock".into()))?.clone();
            Ok(Box::new(LateInstance(0, tx)))
        }
    }

    #[test]
    fn a_static_source_tries_again_after_a_failed_produce() {
        let (tx, rx) = channel();
        let plugin: Arc<dyn Source> = Arc::new(LateFile(Mutex::new(tx)));
        let handle = SourceHandle::spawn(SourceId(1), plugin, Settings::default(), MasterClock::new()).unwrap();
        assert_eq!(rx.recv().unwrap(), 1);
        assert_eq!(rx.recv().unwrap(), 2, "the failed produce is tried again");
        drop(handle);
    }
}
