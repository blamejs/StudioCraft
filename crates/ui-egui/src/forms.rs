//! Forms built from a plug-in's [`SettingsSchema`], and the Properties and Filters windows.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use egui::{Color32, Ui};
use serde_json::{Value, json};
use studiocraft_media::queue::Latest;
use studiocraft_model::collection::{MAX_BALANCE, MAX_SYNC_OFFSET_MS};
use studiocraft_model::{AudioState, FieldKind, FilterId, MonitorMode, PathMode, SettingsSchema, SourceId};
use studiocraft_plugin::{FilterTarget, Registry, Settings};
use studiocraft_studio::Studio;

/// A path field asked for a file or folder dialog.
#[derive(Clone, Debug, PartialEq)]
pub struct Browse {
    pub key: String,
    pub mode: PathMode,
    pub filter_name: String,
    pub extensions: Vec<String>,
}

/// What the user did in a form this frame.
#[derive(Default)]
pub struct FormEvent {
    pub changed: bool,
    pub browse: Option<Browse>,
}

pub(crate) fn color_text(c: [u8; 4]) -> String {
    format!("#{:02x}{:02x}{:02x}{:02x}", c[0], c[1], c[2], c[3])
}

/// Edit `settings` against `schema`; the values the schema does not list are kept.
pub fn form(ui: &mut Ui, id: egui::Id, schema: &SettingsSchema, settings: &mut Settings) -> FormEvent {
    form_locked(ui, id, schema, settings, usize::MAX)
}

/// [`form`] with the fields after index `locked_after` shown but not editable.
fn form_locked(ui: &mut Ui, id: egui::Id, schema: &SettingsSchema, settings: &mut Settings, locked_after: usize) -> FormEvent {
    let mut ev = FormEvent::default();
    egui::Grid::new(id).num_columns(2).spacing([12.0, 6.0]).striped(false).show(ui, |ui| {
        for (index, field) in schema.fields.iter().enumerate() {
            if index == locked_after.saturating_add(1) {
                // Every later row is disabled too.
                ui.disable();
            }
            ui.label(tl!(&field.label));
            let key = field.key.as_str();
            let current = settings.get(key).cloned().unwrap_or_else(|| field.default.clone());
            let changed = match &field.kind {
                FieldKind::Bool => {
                    let mut v = current.as_bool().unwrap_or(false);
                    let r = ui.checkbox(&mut v, "").changed();
                    if r {
                        settings.set(key, v);
                    }
                    r
                }
                FieldKind::Int { min, max } => {
                    let mut v = current.as_i64().unwrap_or(*min).clamp(*min, *max);
                    let r = ui.add(egui::DragValue::new(&mut v).range(*min..=*max)).changed();
                    if r {
                        settings.set(key, v);
                    }
                    r
                }
                FieldKind::Float { min, max, step } => {
                    let mut v = current.as_f64().unwrap_or(*min).clamp(*min, *max);
                    let r = if (max - min) <= 1000.0 {
                        ui.add(egui::Slider::new(&mut v, *min..=*max).step_by(*step)).changed()
                    } else {
                        ui.add(egui::DragValue::new(&mut v).range(*min..=*max).speed(*step)).changed()
                    };
                    if r {
                        settings.set(key, v);
                    }
                    r
                }
                FieldKind::Text { multiline } => {
                    let mut v = current.as_str().unwrap_or_default().to_string();
                    let r = if *multiline {
                        ui.add(egui::TextEdit::multiline(&mut v).desired_rows(3).desired_width(280.0)).changed()
                    } else {
                        ui.add(egui::TextEdit::singleline(&mut v).desired_width(280.0)).changed()
                    };
                    if r {
                        settings.set(key, v);
                    }
                    r
                }
                FieldKind::Font => {
                    let mut v = current.as_str().unwrap_or_default().to_string();
                    let r = ui.add(egui::TextEdit::singleline(&mut v).hint_text(tl!("Font family")).desired_width(280.0)).changed();
                    if r {
                        settings.set(key, v);
                    }
                    r
                }
                FieldKind::Color => {
                    let parsed = Settings::new().with("c", current.clone()).color("c").unwrap_or([255, 255, 255, 255]);
                    let mut c = Color32::from_rgba_unmultiplied(parsed[0], parsed[1], parsed[2], parsed[3]);
                    let r = ui.color_edit_button_srgba(&mut c).changed();
                    if r {
                        settings.set(key, color_text(c.to_srgba_unmultiplied()));
                    }
                    r
                }
                FieldKind::Path { mode, filter_name, extensions } => {
                    let mut v = current.as_str().unwrap_or_default().to_string();
                    let r = ui
                        .horizontal(|ui| {
                            let r = ui.add(egui::TextEdit::singleline(&mut v).desired_width(220.0)).changed();
                            if ui.button(tl!("Browse…")).clicked() {
                                ev.browse = Some(Browse {
                                    key: key.to_string(),
                                    mode: *mode,
                                    filter_name: tl!(filter_name).to_string(),
                                    extensions: extensions.clone(),
                                });
                            }
                            r
                        })
                        .inner;
                    if r {
                        settings.set(key, v);
                    }
                    r
                }
                FieldKind::Choice { options } => {
                    let selected = choice_text(key, &current, options);
                    let mut picked: Option<Value> = None;
                    craft_ui::ComboBox::from_id_salt((id, key)).selected_text(selected).width(280.0).show_ui(ui, |ui| {
                        for o in options {
                            if ui.selectable_label(o.value == current, tl!(&o.label)).clicked() {
                                picked = Some(o.value.clone());
                            }
                        }
                    });
                    match picked {
                        Some(v) if v != current => {
                            settings.set(key, v);
                            true
                        }
                        _ => false,
                    }
                }
            };
            ev.changed |= changed;
            ui.end_row();
        }
    });
    ev
}

/// What a choice field shows for `current`. A saved device that is not in the list shows its name
/// (the part before U+001F, which separates a camera's name from its id) as not connected.
fn choice_text(key: &str, current: &Value, options: &[studiocraft_model::ChoiceOption]) -> String {
    match (options.iter().find(|o| o.value == *current), current.as_str()) {
        (Some(o), _) => tl!(&o.label).to_string(),
        (None, Some(id)) if key == "device" && !id.is_empty() => {
            let name = id.split('\u{1f}').next().unwrap_or(id);
            craft_i18n::fmt(tl!("{device} (not connected)"), &[("device", name)])
        }
        (None, _) => current.to_string(),
    }
}

/// Reads a source's schema for some settings. It can take a while: a camera that has not been
/// read before is opened to list its formats.
type Reader = Arc<dyn Fn(&Settings) -> SettingsSchema + Send + Sync>;

/// A schema being read on its own thread. A window reads one schema at a time: a read asked for
/// while one runs starts when that one ends, and the reply of the earlier one is dropped.
struct Reading {
    /// The devices' stamp taken just before the schema was read, and the schema.
    reply: Arc<Reply<(Option<u64>, SettingsSchema)>>,
    /// Another read was asked for while this one ran.
    again: bool,
    /// The fields after this index stay locked until the reply arrives.
    after: Option<usize>,
    /// A change made in the window waits for the reply: a later choice that the new lists do not
    /// offer goes back to its default, and then the settings are sent. Holds the source's
    /// settings from before that change; when they change elsewhere meanwhile, the change made
    /// elsewhere is kept instead.
    base: Option<Settings>,
}

/// Gives a value that changes when the devices a source lists change; see
/// `Plugin::lists_stamp`.
type Stamper = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

/// How often an open Properties window looks for devices plugged in or out.
const WATCH: Duration = Duration::from_secs(3);

/// Where a read on another thread leaves its value.
pub(crate) struct Reply<T> {
    value: Latest<T>,
    /// Set after the value is left, or when the read stopped without one.
    done: AtomicBool,
}

impl<T> Default for Reply<T> {
    fn default() -> Self {
        Reply { value: Latest::new(), done: AtomicBool::new(false) }
    }
}

impl<T> Reply<T> {
    /// The value once the read has ended: `Some(None)` when it stopped without one.
    pub(crate) fn ended(&self) -> Option<Option<T>> {
        if let Some(value) = self.value.take() {
            return Some(Some(value));
        }
        // The value is left before `done` is set, so it is seen once `done` is.
        self.done.load(Ordering::Acquire).then(|| self.value.take())
    }
}

/// Sets [`Reply::done`] when the read ends, also when the reader panics.
struct Ends<T>(Arc<Reply<T>>);

impl<T> Drop for Ends<T> {
    fn drop(&mut self) {
        self.0.done.store(true, Ordering::Release);
    }
}

/// Run `read` on a thread of its own; its value arrives in the returned reply. When no thread
/// can be started, `read` runs here.
pub(crate) fn read_off<T, F>(read: Arc<F>) -> Arc<Reply<T>>
where
    T: Send + 'static,
    F: Fn() -> T + Send + Sync + ?Sized + 'static,
{
    if let Some(reply) = try_read_off(read.clone()) {
        return reply;
    }
    // The reply is taken like any other.
    let reply = Arc::new(Reply::default());
    reply.value.publish(read());
    reply.done.store(true, Ordering::Release);
    reply
}

/// Run `read` on a thread of its own; its value arrives in the returned reply. `None` when no
/// thread can be started.
pub(crate) fn try_read_off<T, F>(read: Arc<F>) -> Option<Arc<Reply<T>>>
where
    T: Send + 'static,
    F: Fn() -> T + Send + Sync + ?Sized + 'static,
{
    let reply = Arc::new(Reply::default());
    let ends = Ends(reply.clone());
    let started = std::thread::Builder::new().name("studiocraft-properties".into()).spawn(move || {
        // The window may have closed or asked again; then nobody takes it.
        ends.0.value.publish(read());
    });
    started.ok().map(|_| reply)
}

/// Looks for devices plugged in or out while a Properties window is open.
struct Watch {
    /// The devices' stamp at the last look; `None` before the first.
    seen: Option<u64>,
    /// When to look next.
    next: Instant,
    /// A look running on its own thread.
    look: Option<Arc<Reply<Option<u64>>>>,
    /// The source's lists do not change on their own.
    off: bool,
}

/// The Properties window of one source.
pub struct Properties {
    pub source: SourceId,
    /// Read when the window opens (lists of displays, windows and devices are read then), and
    /// again when a choice changes, since one list can depend on another (a camera's formats).
    /// Empty until the first read ends.
    schema: SettingsSchema,
    loaded: bool,
    settings: Settings,
    reader: Reader,
    reading: Option<Reading>,
    stamper: Stamper,
    watch: Watch,
}

impl Properties {
    pub fn open(studio: &Studio, source: SourceId) -> Option<Properties> {
        let s = studio.collection().source(source)?;
        let plugin = studio.registry().source(&s.kind).ok()?;
        let lists = plugin.clone();
        let reader: Reader = Arc::new(move |s: &Settings| plugin.schema_for(s));
        Some(Properties::with_readers(source, s.settings.clone(), reader, Arc::new(move || lists.lists_stamp())))
    }

    #[cfg(test)]
    fn with_reader(source: SourceId, settings: Settings, reader: Reader) -> Properties {
        Properties::with_readers(source, settings, reader, Arc::new(|| None))
    }

    fn with_readers(source: SourceId, settings: Settings, reader: Reader, stamper: Stamper) -> Properties {
        let watch = Watch { seen: None, next: Instant::now(), look: None, off: false };
        let mut p = Properties { source, schema: SettingsSchema::default(), loaded: false, settings, reader, reading: None, stamper, watch };
        p.read(None, None);
        p
    }

    /// Every [`WATCH`], look on another thread for devices plugged in or out, and read the lists
    /// again when they changed. The look waits while the lists are read. True when a read
    /// started.
    fn watch(&mut self, now: Instant) -> bool {
        if self.watch.off {
            return false;
        }
        let Some(look) = &self.watch.look else {
            if now >= self.watch.next && self.loaded && self.reading.is_none() {
                self.watch.look = Some(read_off(self.stamper.clone()));
            }
            return false;
        };
        let Some(stamp) = look.ended() else { return false };
        self.watch.look = None;
        self.watch.next = now + WATCH;
        let Some(stamp) = stamp.flatten() else {
            // The lists do not change on their own, or they could not be looked at.
            self.watch.off = true;
            return false;
        };
        let changed = self.watch.seen.is_some_and(|seen| seen != stamp);
        self.watch.seen = Some(stamp);
        if changed {
            self.read(None, None);
        }
        changed
    }

    /// Read the schema for the current settings on another thread, once the read in progress
    /// ends. Its locks and waiting change carry over.
    fn read(&mut self, after: Option<usize>, base: Option<Settings>) {
        if let Some(r) = &mut self.reading {
            r.after = match (r.after, after) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            r.base = r.base.take().or(base);
            r.again = true;
            return;
        }
        self.start(after, base);
    }

    fn start(&mut self, after: Option<usize>, base: Option<Settings>) {
        let (reader, stamper) = (self.reader.clone(), self.stamper.clone());
        let settings = self.settings.clone();
        // The devices' stamp is taken before the lists: a device plugged in or out while they are
        // read changes the stamp at the next look.
        let reply = read_off(Arc::new(move || (stamper(), reader(&settings))));
        self.reading = Some(Reading { reply, again: false, after, base });
    }

    /// Take a schema that has arrived. When a change made in the window waited for it, the
    /// source's settings from before that change.
    fn poll(&mut self) -> Option<Settings> {
        // A read that stopped without a schema leaves the window with what it has.
        let read = self.reading.as_ref()?.reply.ended()?;
        self.deliver(read).flatten()
    }

    /// A read ended with the devices' stamp and the schema. `None` when the next read has
    /// started, otherwise what [`Properties::poll`] returns.
    fn deliver(&mut self, read: Option<(Option<u64>, SettingsSchema)>) -> Option<Option<Settings>> {
        let r = self.reading.take()?;
        if r.again {
            // The lists were read for settings the window no longer shows.
            self.start(r.after, r.base);
            return None;
        }
        let schema = read.map(|(stamp, schema)| {
            if stamp.is_some() {
                self.watch.seen = stamp;
            }
            schema
        });
        self.arrive(schema, r.after, r.base.is_some());
        Some(r.base)
    }

    fn arrive(&mut self, schema: Option<SettingsSchema>, after: Option<usize>, reset: bool) {
        if let Some(schema) = schema {
            self.schema = schema;
        }
        self.loaded = true;
        if reset {
            self.reset_after(after);
        }
    }

    /// Set every choice after index `after` that the lists do not offer back to its default.
    fn reset_after(&mut self, after: Option<usize>) {
        let Some(at) = after else { return };
        for f in self.schema.fields.iter().skip(at.saturating_add(1)) {
            if let (FieldKind::Choice { options }, Some(v)) = (&f.kind, self.settings.get(&f.key))
                && !options.iter().any(|o| o.value == *v)
            {
                self.settings.set(&f.key, f.default.clone());
            }
        }
    }

    /// When the window closes while a change waits for its lists, the change is sent without
    /// them: the choices after the one changed go back to their defaults, which every list
    /// offers. Returns the settings to send.
    fn close(&mut self, current: &Settings) -> Option<Value> {
        let r = self.reading.take()?;
        if r.base.as_ref() != Some(current) {
            // Nothing waits, or the settings changed elsewhere since.
            return None;
        }
        for f in self.schema.fields.iter().skip(r.after?.saturating_add(1)) {
            if matches!(f.kind, FieldKind::Choice { .. }) {
                self.settings.set(&f.key, f.default.clone());
            }
        }
        Some(self.command())
    }

    /// The index of the first choice whose value differs between `a` and `b`.
    fn changed_choice(&self, a: &Settings, b: &Settings) -> Option<usize> {
        self.schema.fields.iter().position(|f| matches!(f.kind, FieldKind::Choice { .. }) && a.get(&f.key) != b.get(&f.key))
    }

    /// Show `settings` that changed outside the window, reading the schema again when a choice
    /// changed. The values are kept as they are.
    fn take(&mut self, settings: Settings) {
        if !self.loaded {
            // The first lists are still being read, for the settings before this change.
            let changed = settings != self.settings;
            self.settings = settings;
            if changed {
                self.read(None, None);
            }
            return;
        }
        let at = self.changed_choice(&settings, &self.settings);
        self.settings = settings;
        if at.is_some() {
            self.read(at, None);
        }
    }

    /// Read the schema again after the form changed `before` into the current settings, which
    /// were `base` in the source. When a choice changed, the change waits for the new lists
    /// (true): a later choice whose value they do not offer goes back to its default, and the
    /// settings are sent once they arrive.
    fn follow_choice(&mut self, before: &Settings, base: &Settings) -> bool {
        // Another change made while the window waits is sent with it.
        let Some(at) = self.changed_choice(before, &self.settings) else { return self.reading.as_ref().is_some_and(|r| r.base.is_some()) };
        self.read(Some(at), Some(base.clone()));
        true
    }

    /// Point a path field at a chosen file.
    pub fn set(&mut self, key: &str, value: &str) -> Value {
        self.settings.set(key, value);
        self.command()
    }

    fn command(&self) -> Value {
        json!({"source": self.source, "settings": self.settings})
    }

    /// Draw the window; the `source.settings` parameters to run when a value changed, a
    /// requested file dialog, and the audio commands to run. `open` turns false when the window
    /// is closed.
    pub fn show(&mut self, ctx: &egui::Context, studio: &Studio, open: &mut bool) -> (Option<Value>, Option<Browse>, Vec<Action>) {
        let Some(src) = studio.collection().source(self.source) else {
            *open = false;
            return (None, None, Vec::new());
        };
        let has_audio = studio.registry().source(&src.kind).is_ok_and(|p| p.caps().audio);
        let mut audio = Vec::new();
        let mut out = (None, None);
        // A change that waited is sent unless the settings changed elsewhere meanwhile; then the
        // change made elsewhere is shown instead.
        if self.poll().is_some_and(|base| base == src.settings) {
            out.0 = Some(self.command());
        }
        self.watch(Instant::now());
        // The source's settings once this frame's commands have run: a change that starts
        // waiting in this frame is compared with them.
        let base = if out.0.is_some() { self.settings.clone() } else { src.settings.clone() };
        let waiting = self.reading.as_ref().is_some_and(|r| r.base.is_some());
        if out.0.is_none() && !waiting && src.settings != self.settings && !ctx.egui_is_using_pointer() && !ctx.egui_wants_keyboard_input() {
            // Undo, a script or the control channel changed them.
            self.take(src.settings.clone());
        }
        let title = craft_i18n::fmt(tl!("Properties for '{name}'"), &[("name", &src.name)]);
        let error = studio.source_error(self.source);
        let notice = studio.source_notice(self.source);
        let mut refresh = false;
        let locked_after = match &self.reading {
            Some(Reading { after: Some(at), .. }) => *at,
            _ => usize::MAX,
        };
        let window = egui::Window::new(title).id(egui::Id::unique(("properties", self.source.0))).open(open).resizable(true).default_width(440.0);
        window.show(ctx, |ui| {
            if let Some(e) = &error {
                ui.colored_label(ui.visuals().error_fg_color, crate::i18n::Msg::plain(e.to_string()).shown());
                ui.separator();
            } else if let Some(n) = notice {
                ui.colored_label(ui.visuals().warn_fg_color, crate::i18n::Msg::plain(n).shown());
                ui.separator();
            }
            if self.reading.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak(tl!("Reading the lists…"));
                });
            }
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                let before = self.settings.clone();
                let ev = form_locked(ui, egui::Id::unique(("props-form", self.source.0)), &self.schema, &mut self.settings, locked_after);
                if ev.changed && !self.follow_choice(&before, &base) {
                    out.0 = Some(self.command());
                }
                out.1 = ev.browse;
                if has_audio {
                    ui.separator();
                    audio = audio_form(ui, self.source, &src.audio, studio.filter_latency_frames(self.source));
                }
            });
            ui.separator();
            ui.horizontal(|ui| {
                // Until the first lists arrive there are no defaults to go back to.
                if ui.add_enabled(self.loaded, egui::Button::new(tl!("Defaults"))).clicked() {
                    let before = std::mem::replace(&mut self.settings, self.schema.defaults());
                    if !self.follow_choice(&before, &base) {
                        out.0 = Some(self.command());
                    }
                }
                if ui.button(tl!("Refresh Lists")).on_hover_text(tl!("Read the displays, windows and devices again")).clicked() {
                    refresh = true;
                }
            });
        });
        if refresh {
            self.read(None, None);
        }
        if !*open && let Some(cmd) = self.close(&base) {
            out.0 = Some(cmd);
        }
        (out.0, out.1, audio)
    }
}

/// The audio rows of a source's Properties window, with the delay in sample frames that the
/// source's running audio filters add: the commands to run for what changed.
fn audio_form(ui: &mut Ui, source: SourceId, state: &AudioState, latency_frames: usize) -> Vec<Action> {
    let mut actions = Vec::new();
    ui.strong(tl!("Audio"));
    egui::Grid::new(egui::Id::unique(("props-audio", source.0))).num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
        ui.label(tl!("Sync offset"));
        let mut ms = state.sync_offset_ms.min(MAX_SYNC_OFFSET_MS);
        let r = ui
            .add(egui::DragValue::new(&mut ms).range(0..=MAX_SYNC_OFFSET_MS).suffix(format!(" {}", tl!("ms"))))
            .on_hover_text(tl!("Delays this source's audio, to line it up with its picture"));
        if r.changed() && ms != state.sync_offset_ms {
            actions.push(("source.sync_offset", json!({"source": source, "ms": ms})));
        }
        ui.end_row();
        ui.label(tl!("Filter delay"));
        let frames = latency_frames.to_string();
        ui.label(latency_text(latency_frames)).on_hover_text(craft_i18n::fmt(
            tl!("The audio filters on this source delay its sound by {frames} samples at 48 kHz"),
            &[("frames", &frames)],
        ));
        ui.end_row();
        ui.label(tl!("Monitoring")).on_hover_text(tl!("Plays this source on the monitoring device chosen in Settings › Audio"));
        let mut mode = state.monitor;
        craft_ui::ComboBox::from_id_salt(("props-monitor", source.0)).selected_text(monitor_label(mode)).width(200.0).show_ui(ui, |ui| {
            for m in MonitorMode::ALL {
                ui.selectable_value(&mut mode, m, monitor_label(m));
            }
        });
        if mode != state.monitor {
            actions.push(("source.monitor", json!({"source": source, "mode": mode.id()})));
        }
        ui.end_row();
        ui.label("");
        let mut own = state.records_track();
        let r = ui
            .checkbox(&mut own, tl!("Record as its own track"))
            .on_hover_text(tl!("Audio tracks write this source to a WAV file of its own (Settings › Output › Audio tracks)"));
        if r.changed() {
            actions.push(("source.set_record_track", json!({"source": source, "on": own})));
        }
        ui.end_row();
        ui.label(tl!("Balance"));
        let mut balance = if state.balance.is_finite() { state.balance.clamp(-MAX_BALANCE, MAX_BALANCE) } else { 0.0 };
        ui.horizontal(|ui| {
            // `Edits` leaves a saved value between the 0.01 steps as it is when the slider draws;
            // a drag or a key press still rounds to a step and stays in range.
            let slider =
                egui::Slider::new(&mut balance, -MAX_BALANCE..=MAX_BALANCE).step_by(0.01).clamping(egui::SliderClamping::Edits).show_value(false);
            let r = ui.add(slider).on_hover_text(tl!("Moves this source between the left and right channels; double-click to center it"));
            if r.double_clicked() {
                balance = 0.0;
            }
            ui.label(balance_text(balance));
        });
        if balance != state.balance {
            actions.push(("source.balance", json!({"source": source, "balance": balance})));
        }
        ui.end_row();
        ui.label("");
        let mut mono = state.mono;
        let r =
            ui.checkbox(&mut mono, tl!("Downmix to mono")).on_hover_text(tl!("Plays the average of this source's left and right channels on both"));
        if r.changed() {
            actions.push(("source.mono", json!({"source": source, "on": mono})));
        }
        ui.end_row();
    });
    actions
}

/// A balance as the Properties window shows it: centered, or how far toward one side.
fn balance_text(balance: f32) -> String {
    let percent = (balance.abs().min(MAX_BALANCE) * 100.0).round();
    if percent == 0.0 {
        return tl!("Center").to_string();
    }
    let percent = format!("{percent:.0}");
    let text = if balance < 0.0 { tl!("{percent}% left") } else { tl!("{percent}% right") };
    craft_i18n::fmt(text, &[("percent", &percent)])
}

/// A filter delay of `frames` sample frames as the Properties window shows it, in milliseconds.
fn latency_text(frames: usize) -> String {
    format!("{:.1} {}", studiocraft_studio::latency_ms(frames), tl!("ms"))
}

/// A monitoring mode as the Properties window shows it.
fn monitor_label(mode: MonitorMode) -> String {
    match mode {
        MonitorMode::Off => tl!("Monitor off").to_string(),
        MonitorMode::MonitorOnly => tl!("Monitor only (left out of recordings and streams)").to_string(),
        MonitorMode::MonitorAndOutput => tl!("Monitor and output").to_string(),
    }
}

/// The Filters window of one source.
pub struct Filters {
    pub source: SourceId,
    selected: Option<FilterId>,
    /// The selected filter's schema and settings.
    editing: Option<(FilterId, SettingsSchema, Settings)>,
}

/// A change made in the Filters window: a command id and its parameters.
pub type Action = (&'static str, Value);

impl Filters {
    pub fn new(source: SourceId) -> Filters {
        Filters { source, selected: None, editing: None }
    }

    fn kinds(registry: &Registry, video: bool, audio: bool) -> Vec<(&'static str, &'static str)> {
        registry
            .filters()
            .iter()
            .filter(|f| match f.target() {
                FilterTarget::Video => video,
                FilterTarget::Audio => audio,
            })
            .map(|f| (f.id(), tl!(f.name())))
            .collect()
    }

    pub fn show(&mut self, ctx: &egui::Context, studio: &Studio, open: &mut bool) -> Vec<Action> {
        let mut actions = Vec::new();
        let Some(src) = studio.collection().source(self.source) else {
            *open = false;
            return actions;
        };
        let registry = studio.registry();
        let caps = registry.source(&src.kind).map(|p| p.caps()).ok();
        let (video, audio) = caps.map_or((true, true), |c| (c.video, c.audio));
        if self.selected.is_none_or(|id| !src.filters.iter().any(|f| f.id == id)) {
            self.selected = src.filters.first().map(|f| f.id);
        }
        let selected = self.selected.and_then(|id| src.filters.iter().find(|f| f.id == id));
        match (selected, &self.editing) {
            (Some(f), Some((id, _, s))) if *id == f.id && (*s == f.settings || ctx.egui_is_using_pointer() || ctx.egui_wants_keyboard_input()) => {}
            (Some(f), _) => {
                let schema = registry.filter(&f.kind).map(|p| p.schema()).unwrap_or_default();
                self.editing = Some((f.id, schema, f.settings.clone()));
            }
            (None, _) => self.editing = None,
        }
        let source = self.source;
        let title = craft_i18n::fmt(tl!("Filters for '{name}'"), &[("name", &src.name)]);
        egui::Window::new(title).id(egui::Id::unique(("filters", source.0))).open(open).default_size([620.0, 360.0]).show(ctx, |ui| {
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    ui.set_width(200.0);
                    for f in &src.filters {
                        ui.horizontal(|ui| {
                            let mut on = f.enabled;
                            if ui.checkbox(&mut on, "").on_hover_text(tl!("Enabled")).changed() {
                                actions.push(("filter.enable", json!({"source": source, "filter": f.id.0, "enabled": on})));
                            }
                            if ui.selectable_label(self.selected == Some(f.id), &f.name).clicked() {
                                self.selected = Some(f.id);
                            }
                        });
                    }
                    if src.filters.is_empty() {
                        ui.weak(tl!("No filters yet."));
                    }
                    ui.separator();
                    ui.horizontal(|ui| {
                        ui.menu_button(format!("{} ▾", tl!("Add")), |ui| {
                            for (kind, name) in Self::kinds(registry, video, audio) {
                                if ui.button(name).clicked() {
                                    actions.push(("filter.add", json!({"source": source, "kind": kind})));
                                    ui.close();
                                }
                            }
                        });
                        if let Some(id) = self.selected
                            && ui.button(tl!("Remove")).clicked()
                        {
                            actions.push(("filter.remove", json!({"source": source, "filter": id.0})));
                        }
                    });
                });
                ui.separator();
                ui.vertical(|ui| {
                    if let Some((id, schema, settings)) = &mut self.editing {
                        if schema.fields.is_empty() {
                            ui.weak(tl!("This filter has no settings."));
                        }
                        let ev = form(ui, egui::Id::unique(("filter-form", id.0)), schema, settings);
                        if ev.changed {
                            actions.push(("filter.settings", json!({"source": source, "filter": id.0, "settings": settings})));
                        }
                    } else {
                        ui.weak(tl!("Add a filter to change how this source looks or sounds."));
                    }
                });
            });
        });
        actions
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use studiocraft_model::{ChoiceOption, Field};

    use super::*;

    fn schema() -> SettingsSchema {
        SettingsSchema::new(vec![
            Field::new("on", "On", FieldKind::Bool, true),
            Field::new("n", "Count", FieldKind::Int { min: 0, max: 10 }, 3),
            Field::new("c", "Color", FieldKind::Color, "#ff0000ff"),
            Field::new("mode", "Mode", FieldKind::Choice { options: vec![ChoiceOption::new("a", "A"), ChoiceOption::new("b", "B")] }, "a"),
        ])
    }

    #[test]
    fn a_form_draws_every_kind_without_changing_anything() {
        let ctx = egui::Context::default();
        let mut settings = Settings::new().with("extra", "kept");
        let mut changed = true;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            changed = form(ui, egui::Id::unique("t"), &schema(), &mut settings).changed;
        });
        out.textures_delta.clear();
        assert!(!changed);
        assert_eq!(settings.str("extra"), Some("kept"));
    }

    #[test]
    fn a_saved_device_that_is_absent_shows_as_not_connected() {
        let options = [ChoiceOption::new("", "Default"), ChoiceOption::new("Desk\u{1f}1", "Desk")];
        assert_eq!(choice_text("device", &json!("Desk\u{1f}1"), &options), "Desk");
        let gone = craft_i18n::fmt(tl!("{device} (not connected)"), &[("device", "Room")]);
        assert_eq!(choice_text("device", &json!("Room\u{1f}2"), &options), gone, "the name without the id");
        assert_eq!(choice_text("device", &json!("Room"), &options), craft_i18n::fmt(tl!("{device} (not connected)"), &[("device", "Room")]));
        assert_eq!(choice_text("mode", &json!("x"), &options), "\"x\"", "other fields show the value");
    }

    /// A schema whose `format` choices are those of the chosen `device`.
    fn camera(s: &Settings) -> SettingsSchema {
        let formats: &[&str] = if s.str("device") == Some("a") { &["", "hd"] } else { &["", "sd"] };
        SettingsSchema::new(vec![
            Field::new("device", "Device", FieldKind::Choice { options: vec![ChoiceOption::new("a", "A"), ChoiceOption::new("b", "B")] }, ""),
            Field::new("format", "Format", FieldKind::Choice { options: formats.iter().map(|f| ChoiceOption::new(*f, *f)).collect() }, ""),
        ])
    }

    /// Wait for the read in progress and take its schema; whether the settings are to be sent.
    fn finish(p: &mut Properties) -> bool {
        loop {
            let reply = p.reading.as_ref().expect("a read in progress").reply.clone();
            let read = loop {
                if let Some(read) = reply.ended() {
                    break read;
                }
                std::thread::yield_now();
            };
            if let Some(base) = p.deliver(read) {
                return base.is_some();
            }
        }
    }

    /// A Properties window over [`camera`] whose first lists have arrived.
    fn opened(start: &Settings) -> Properties {
        let mut p = Properties::with_reader(SourceId(1), start.clone(), Arc::new(camera));
        assert!(!finish(&mut p), "opening sends nothing");
        p
    }

    #[test]
    fn a_new_device_reads_its_formats_and_drops_one_it_does_not_offer() {
        let start = Settings::new().with("device", "a").with("format", "hd");
        let mut p = opened(&start);
        p.settings.set("format", "");
        assert!(p.follow_choice(&start, &start));
        assert!(finish(&mut p));
        p.settings.set("format", "hd");
        let before = p.settings.clone();
        p.settings.set("device", "b");
        assert!(p.follow_choice(&before, &before), "the change waits for the new device's formats");
        assert_eq!(p.reading.as_ref().and_then(|r| r.after), Some(0), "the format is locked meanwhile");
        assert_eq!(p.settings.str("format"), Some("hd"));
        assert!(finish(&mut p), "and is sent once they arrive");
        assert_eq!(p.settings.str("format"), Some(""), "the new device does not offer hd");
        let FieldKind::Choice { options } = &p.schema.fields[1].kind else { panic!("format is a choice") };
        assert_eq!(options[1].value, json!("sd"));
        // A change to a later field keeps the earlier ones.
        let gone = Settings::new().with("device", "c").with("format", "");
        let mut p = opened(&gone);
        p.settings.set("format", "sd");
        assert!(p.follow_choice(&gone, &gone));
        assert!(finish(&mut p));
        assert_eq!(p.settings.str("device"), Some("c"), "an absent device is kept");
        assert_eq!(p.settings.str("format"), Some("sd"));
    }

    #[test]
    fn a_device_changed_by_undo_or_a_script_lists_its_formats() {
        let start = Settings::new().with("device", "b").with("format", "sd");
        let mut p = opened(&start);
        p.take(Settings::new().with("device", "a").with("format", "hd"));
        assert!(!finish(&mut p), "a change made elsewhere is not sent back");
        let FieldKind::Choice { options } = &p.schema.fields[1].kind else { panic!("format is a choice") };
        assert_eq!(options[1].value, json!("hd"), "the formats of the device the settings now name");
        assert_eq!(p.settings.str("format"), Some("hd"), "the values are kept");
    }

    #[test]
    fn the_lists_are_read_off_the_window_thread() {
        let window = std::thread::current().id();
        let readers = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (open_gate, gate) = mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        let seen = readers.clone();
        let reader: Reader = Arc::new(move |s: &Settings| {
            seen.lock().unwrap().push(std::thread::current().id());
            // A slow camera: the read ends when the test lets it.
            let _ = gate.lock().unwrap().recv();
            camera(s)
        });
        open_gate.send(()).unwrap();
        let mut p = Properties::with_reader(SourceId(1), Settings::new().with("device", "a"), reader);
        finish(&mut p);
        assert_eq!(readers.lock().unwrap().len(), 1);
        assert!(readers.lock().unwrap().iter().all(|t| *t != window), "the lists are not read on the window's thread");
        // While a read is held up, the window has the lists it had and does not wait.
        p.take(Settings::new().with("device", "b"));
        assert!(p.poll().is_none(), "nothing has arrived yet");
        assert!(p.reading.is_some(), "the read is still running");
        let FieldKind::Choice { options } = &p.schema.fields[1].kind else { panic!("format is a choice") };
        assert_eq!(options[1].value, json!("hd"), "the earlier lists stay shown");
        open_gate.send(()).unwrap();
        finish(&mut p);
        let FieldKind::Choice { options } = &p.schema.fields[1].kind else { panic!("format is a choice") };
        assert_eq!(options[1].value, json!("sd"));
    }

    #[test]
    fn a_reply_to_a_replaced_read_is_dropped() {
        let (open_gate, gate) = mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        // Device "a" is slow to give its formats; device "b" answers at once.
        let reader: Reader = Arc::new(move |s: &Settings| {
            if s.str("device") == Some("a") {
                let _ = gate.lock().unwrap().recv();
            }
            camera(s)
        });
        let start = Settings::new().with("device", "b").with("format", "sd");
        let mut p = Properties::with_reader(SourceId(1), start.clone(), reader);
        finish(&mut p);
        p.settings.set("device", "a");
        assert!(p.follow_choice(&start, &start));
        let before = p.settings.clone();
        p.settings.set("device", "b");
        assert!(p.follow_choice(&before, &start));
        // The slow read of "a" ends first; its lists are not the ones the window now needs.
        open_gate.send(()).unwrap();
        assert!(finish(&mut p));
        let FieldKind::Choice { options } = &p.schema.fields[1].kind else { panic!("format is a choice") };
        assert_eq!(options[1].value, json!("sd"), "the formats of the device chosen last");
        assert_eq!(p.settings.str("format"), Some("sd"));
        assert!(p.poll().is_none(), "no later reply replaces them");
    }

    /// Wait for the look in progress to end.
    fn looked(p: &Properties) {
        let look = p.watch.look.clone().expect("a look in progress");
        while !look.done.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
    }

    #[test]
    fn a_device_plugged_in_or_out_while_the_window_is_open_reads_the_lists_again() {
        let devices = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let mut p = opened(&Settings::new().with("device", "a").with("format", "hd"));
        let theirs = devices.clone();
        p.stamper = Arc::new(move || Some(theirs.load(Ordering::Acquire)));
        let t = Instant::now();
        p.watch.next = t;
        assert!(!p.watch(t));
        looked(&p);
        assert!(!p.watch(t), "the first look only notes the devices");
        assert!(p.reading.is_none());
        devices.store(2, Ordering::Release);
        assert!(!p.watch(t + WATCH - Duration::from_millis(1)));
        assert!(p.watch.look.is_none(), "the next look waits");
        assert!(!p.watch(t + WATCH));
        looked(&p);
        assert!(p.watch(t + WATCH), "a camera was plugged in or out");
        assert!(!finish(&mut p), "the lists are read again and nothing is sent");
        assert_eq!(p.settings.str("format"), Some("hd"), "the values are kept");
        assert!(!p.watch(t + WATCH + WATCH));
        looked(&p);
        assert!(!p.watch(t + WATCH + WATCH), "the same devices read nothing");
        assert!(p.reading.is_none());
    }

    #[test]
    fn a_device_plugged_in_before_the_first_look_reads_the_lists_again() {
        let devices = Arc::new(std::sync::atomic::AtomicU64::new(1));
        let theirs = devices.clone();
        let stamper: Stamper = Arc::new(move || Some(theirs.load(Ordering::Acquire)));
        let mut p = Properties::with_readers(SourceId(1), Settings::new().with("device", "a"), Arc::new(camera), stamper);
        finish(&mut p);
        // Plugged in after the lists were read and before the window looked.
        devices.store(2, Ordering::Release);
        let t = Instant::now();
        p.watch.next = t;
        assert!(!p.watch(t));
        looked(&p);
        assert!(p.watch(t), "the lists were read for other devices");
    }

    #[test]
    fn lists_that_do_not_change_on_their_own_are_not_watched() {
        let mut p = opened(&Settings::new().with("device", "a"));
        let t = Instant::now();
        p.watch.next = t;
        assert!(!p.watch(t));
        looked(&p);
        assert!(!p.watch(t));
        assert!(!p.watch(t + WATCH + WATCH));
        assert!(p.watch.look.is_none(), "no more looks");
    }

    #[test]
    fn a_look_waits_while_the_lists_are_read() {
        let (open_gate, reader) = held();
        let mut p = Properties::with_reader(SourceId(1), Settings::new().with("device", "a"), reader);
        p.stamper = Arc::new(|| Some(1));
        let t = Instant::now();
        p.watch.next = t;
        assert!(!p.watch(t));
        assert!(p.watch.look.is_none(), "the first lists are still being read");
        open_gate.send(()).unwrap();
        finish(&mut p);
        assert!(!p.watch(t));
        assert!(p.watch.look.is_some());
    }

    /// A reader for [`camera`] that waits until the test sends on the returned channel.
    fn held() -> (mpsc::Sender<()>, Reader) {
        let (open_gate, gate) = mpsc::channel::<()>();
        let gate = std::sync::Mutex::new(gate);
        let reader: Reader = Arc::new(move |s: &Settings| {
            let _ = gate.lock().unwrap().recv();
            camera(s)
        });
        (open_gate, reader)
    }

    #[test]
    fn a_change_made_elsewhere_while_the_first_lists_are_read_reads_them_again() {
        let (open_gate, reader) = held();
        let mut p = Properties::with_reader(SourceId(1), Settings::new().with("device", "b"), reader);
        p.take(Settings::new().with("device", "a"));
        open_gate.send(()).unwrap();
        open_gate.send(()).unwrap();
        finish(&mut p);
        let FieldKind::Choice { options } = &p.schema.fields[1].kind else { panic!("format is a choice") };
        assert_eq!(options[1].value, json!("hd"), "the formats of the device the settings name now");
    }

    #[test]
    fn a_waiting_change_records_the_settings_it_was_made_on() {
        let start = Settings::new().with("device", "b").with("format", "sd");
        let (open_gate, reader) = held();
        open_gate.send(()).unwrap();
        let mut p = Properties::with_reader(SourceId(1), start.clone(), reader);
        finish(&mut p);
        p.settings.set("device", "a");
        assert!(p.follow_choice(&start, &start));
        open_gate.send(()).unwrap();
        // The reply is polled the way the window polls it.
        let base = loop {
            if let Some(base) = p.poll() {
                break base;
            }
            assert!(p.reading.is_some(), "the read is still running");
            std::thread::yield_now();
        };
        assert_eq!(base, start, "compared with the source's settings before the change is sent");
        assert_eq!(p.settings.str("format"), Some(""), "the new device does not offer sd");
    }

    #[test]
    fn closing_the_window_while_a_change_waits_sends_it_with_default_choices_after_it() {
        let start = Settings::new().with("device", "b").with("format", "sd");
        let (open_gate, reader) = held();
        open_gate.send(()).unwrap();
        let mut p = Properties::with_reader(SourceId(1), start.clone(), reader);
        finish(&mut p);
        p.settings.set("device", "a");
        assert!(p.follow_choice(&start, &start));
        let sent = p.close(&start).expect("the waiting change is sent");
        assert_eq!(sent["settings"]["device"], json!("a"));
        assert_eq!(sent["settings"]["format"], json!(""), "a format every camera offers");
        // When the settings changed elsewhere meanwhile, the change made elsewhere is kept.
        let mut p = Properties::with_reader(SourceId(1), start.clone(), held().1);
        p.reading = None;
        p.loaded = true;
        p.schema = camera(&start);
        p.settings.set("device", "a");
        assert!(p.follow_choice(&start, &start));
        assert_eq!(p.close(&Settings::new().with("device", "b")), None);
        drop(open_gate);
    }

    #[test]
    fn the_audio_rows_draw_without_running_a_command() {
        let ctx = egui::Context::default();
        let mut actions = vec![("x", json!(null))];
        let state = AudioState { sync_offset_ms: 250, ..AudioState::default() };
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
            actions = audio_form(ui, SourceId(3), &state, 248);
        });
        out.textures_delta.clear();
        assert!(actions.is_empty());
    }

    #[test]
    fn the_filter_delay_is_shown_in_milliseconds() {
        // The unit is translated; other tests may switch the language meanwhile.
        for (frames, shown) in [(0, "0.0 "), (248, "5.2 "), (48_000, "1000.0 ")] {
            assert!(latency_text(frames).starts_with(shown), "{frames}: {}", latency_text(frames));
        }
    }

    #[test]
    fn every_monitoring_mode_draws_with_its_own_label() {
        let ctx = egui::Context::default();
        for mode in MonitorMode::ALL {
            let state = AudioState { monitor: mode, ..AudioState::default() };
            let mut actions = vec![("x", json!(null))];
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                actions = audio_form(ui, SourceId(3), &state, 0);
            });
            out.textures_delta.clear();
            assert!(actions.is_empty(), "{mode:?}");
        }
        let labels: std::collections::HashSet<String> = MonitorMode::ALL.into_iter().map(monitor_label).collect();
        assert_eq!(labels.len(), 3);
    }

    #[test]
    fn the_balance_and_mono_rows_draw_without_running_a_command() {
        let ctx = egui::Context::default();
        // A saved balance between the slider's 0.01 steps (0.347, -0.123) stays as it is.
        for (balance, mono) in [(0.0, false), (-1.0, true), (0.35, false), (1.0, true), (0.347, false), (-0.123, true)] {
            let state = AudioState { balance, mono, ..AudioState::default() };
            let mut actions = vec![("x", json!(null))];
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                actions = audio_form(ui, SourceId(3), &state, 0);
            });
            out.textures_delta.clear();
            assert!(actions.is_empty(), "{balance} {mono}: {actions:?}");
        }
    }

    #[test]
    fn the_balance_is_shown_as_a_side_and_a_percentage() {
        // The words are translated; other tests may switch the language meanwhile.
        for (balance, number) in [(-0.25, "25"), (0.5, "50"), (1.0, "100"), (-1.0, "100"), (0.004, ""), (-0.004, ""), (0.0, "")] {
            let text = balance_text(balance);
            assert_eq!(text.contains(number) && !number.is_empty(), !number.is_empty(), "{balance}: {text}");
        }
        assert_ne!(balance_text(-0.25), balance_text(0.25), "the two sides read differently");
        assert_eq!(balance_text(0.0), balance_text(-0.0), "the center reads the same from either side");
    }

    #[test]
    fn colors_are_written_as_hex_with_alpha() {
        assert_eq!(color_text([255, 16, 0, 128]), "#ff100080");
    }
}
