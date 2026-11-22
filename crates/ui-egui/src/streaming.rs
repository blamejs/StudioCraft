//! The Streaming window (View › Streaming): every stream destination of the profile with its on or
//! off setting, its server with the key hidden, where its key comes from and, while the stream
//! runs, what it is doing. Each change runs one of the window's `stream.destination*` commands.

use craft_ui::contract::{Bind, Entry, Props as _, Show as _};
use craft_ui::pickers::Select;
use egui::{RichText, Ui};
use serde_json::{Value, json};
use studiocraft_model::profile::{MAX_DESTINATION_ID, MAX_DESTINATIONS, PRIMARY_DESTINATION};
use studiocraft_model::{Destination, KeyRef, StreamSettings};
use studiocraft_plugin::{DestinationState, DestinationStatus};
use studiocraft_studio::OutputKind;

use crate::i18n::Msg;
use crate::kit::tokens::Palette;
use crate::settings::key_source;
use crate::theme;
use crate::{Act, StudioApp};

/// Longest destination name, in characters.
pub const MAX_NAME: usize = 128;
/// Longest server address, in bytes.
pub const MAX_SERVER: usize = 4096;

/// What the Streaming window keeps between frames: the destination whose details are open, with
/// the text typed into them.
#[derive(Default)]
pub(crate) struct StreamingPanel {
    pub(crate) editing: Option<Edit>,
    /// The name of the profile the open details belong to.
    pub(crate) profile: String,
}

/// The open details of one destination. Nothing changes until Save.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Edit {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) server: String,
    pub(crate) key: KeyChoice,
    /// The name a key for this session is held under.
    pub(crate) held_name: String,
    /// A key being typed to hold for this session; it is never saved.
    pub(crate) held_key: String,
    /// Whether the server field shows its text.
    pub(crate) show_server: bool,
}

/// Where a destination's key comes from, as the Key choice offers it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum KeyChoice {
    #[default]
    None,
    Profile,
    Held,
}

impl KeyChoice {
    pub(crate) const ALL: [KeyChoice; 3] = [KeyChoice::Profile, KeyChoice::Held, KeyChoice::None];

    /// The name the `key` parameter uses.
    pub(crate) fn name(self) -> &'static str {
        match self {
            KeyChoice::None => "none",
            KeyChoice::Profile => "profile",
            KeyChoice::Held => "held",
        }
    }

    /// The choice's English label.
    pub(crate) fn label(self) -> &'static str {
        match self {
            KeyChoice::None => "None (a key at the end of the server address)",
            KeyChoice::Profile => "The profile's stream key",
            KeyChoice::Held => "A key for this session only",
        }
    }

    fn of(key: &KeyRef) -> KeyChoice {
        match key {
            KeyRef::None => KeyChoice::None,
            KeyRef::Profile => KeyChoice::Profile,
            KeyRef::Held(_) => KeyChoice::Held,
        }
    }
}

impl Edit {
    fn of(d: &Destination) -> Edit {
        let held_name = if let KeyRef::Held(n) = &d.key { n.clone() } else { d.id.clone() };
        Edit { id: d.id.clone(), name: d.name.clone(), server: d.server.clone(), key: KeyChoice::of(&d.key), held_name, ..Edit::default() }
    }

    /// The `stream.destination_set` parameters that save these details.
    pub(crate) fn params(&self) -> Value {
        let mut p = json!({"id": self.id, "name": self.name, "server": self.server, "key": self.key.name()});
        if self.key == KeyChoice::Held {
            p["held_name"] = json!(self.held_name);
        }
        p
    }
}

/// `server` as the window shows it: a user name and password before the host, a part after the
/// application, which can be a key, and a query are shown as dots, whichever key source the
/// destination uses.
pub(crate) fn shown_server(server: &str) -> String {
    let s = server.trim();
    let hidden = match user_info(s) {
        Some((start, at)) => format!("{}•••{}", s.get(..start).unwrap_or(""), s.get(at..).unwrap_or("")),
        None => s.to_string(),
    };
    let (s, query) = match hidden.split_once('?') {
        Some((a, _)) => (a, true),
        None => (hidden.as_str(), false),
    };
    let base = s.trim_end_matches('/');
    let mut out = match split_name(base) {
        Some(head) => format!("{head}/•••"),
        None => base.to_string(),
    };
    if query {
        out.push_str("?•••");
    }
    out
}

/// The address before its last part when the host is followed by an application and a name, as
/// the stream reads a key written at the end of the address.
fn split_name(url: &str) -> Option<&str> {
    let start = url.find("://")?.checked_add(3)?;
    let (base, name) = url.rsplit_once('/')?;
    let (_, app) = base.get(start..)?.split_once('/')?;
    (!app.is_empty() && !name.is_empty()).then_some(base)
}

/// Where the user name of `url` starts and where the last `@` of its authority is, when the
/// authority (the text between `://` and the next `/`) has one, as in `rtmp://user:pass@host/app`.
fn user_info(url: &str) -> Option<(usize, usize)> {
    let start = url.find("://")?.checked_add(3)?;
    let rest = url.get(start..)?;
    let authority = rest.split('/').next().unwrap_or(rest);
    let at = authority.rfind('@')?;
    Some((start, start.checked_add(at)?))
}

/// Where a destination's key comes from, as the Streaming window says it.
fn panel_key_source(key: &KeyRef) -> String {
    match key {
        KeyRef::Held(name) => craft_i18n::fmt(tl!("Held for this session as {name} and never saved"), &[("name", name)]),
        other => key_source(other),
    }
}

/// How a status line is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    Plain,
    Good,
    Warn,
}

/// What destination `d` is doing, in the interface language: from `live` while the stream runs,
/// otherwise whether starting the stream sends to it.
pub(crate) fn status_text(d: &Destination, running: bool, live: Option<&DestinationState>) -> (String, Tone) {
    let error = |e: &str| Msg::plain(e).shown();
    match (running, live) {
        (true, Some(s)) => match &s.status {
            DestinationStatus::Connecting => (tl!("Connecting").to_string(), Tone::Plain),
            DestinationStatus::Live => (tl!("Live").to_string(), Tone::Good),
            DestinationStatus::Retrying { attempt, error: e } => (
                craft_i18n::fmt(tl!("Trying again (try {attempt}): {error}"), &[("attempt", &attempt.to_string()), ("error", &error(e))]),
                Tone::Warn,
            ),
            DestinationStatus::Stopped => (tl!("Stopped").to_string(), Tone::Plain),
            DestinationStatus::Failed(e) => (craft_i18n::fmt(tl!("Failed: {error}"), &[("error", &error(e))]), Tone::Warn),
        },
        (true, None) => (tl!("Not in this stream; it is sent to from the next start").to_string(), Tone::Plain),
        (false, _) if d.enabled => (tl!("Sent to when the stream starts").to_string(), Tone::Plain),
        (false, _) => (tl!("Off").to_string(), Tone::Plain),
    }
}

/// Whether a destination in this state is sending or trying to.
fn sending(s: &DestinationState) -> bool {
    !matches!(s.status, DestinationStatus::Stopped | DestinationStatus::Failed(_))
}

/// What the Streaming window shows.
pub(crate) struct View<'a> {
    pub(crate) stream: &'a StreamSettings,
    /// Whether a stream runs.
    pub(crate) running: bool,
    /// The destinations of the running stream.
    pub(crate) live: &'a [DestinationState],
    /// Whether a key is held for this session under a name.
    pub(crate) held: &'a dyn Fn(&str) -> bool,
}

/// The Streaming window's contents.
pub(crate) fn show(ui: &mut Ui, view: &View, panel: &mut StreamingPanel, acts: &mut Vec<Act>) {
    let palette = theme::palette(ui.ctx());
    let count = view.stream.destinations.len();
    ui.horizontal(|ui| {
        let full = count >= MAX_DESTINATIONS;
        let add = ui.add_enabled(!full, egui::Button::new(tl!("Add Destination")));
        if add.clicked() {
            acts.push(Act::run("stream.destination_add", json!({})));
        }
        ui.weak(craft_i18n::fmt(tl!("{n} of {max} destinations"), &[("n", &count.to_string()), ("max", &MAX_DESTINATIONS.to_string())]));
    });
    if count == 0 {
        ui.weak(tl!("No destinations yet. Add one to stream to it."));
    }
    if view.running {
        ui.weak(tl!("Server and key changes are used from the next start of the stream."));
    }
    ui.separator();
    egui::ScrollArea::vertical().id_salt("streaming").auto_shrink([false, true]).show(ui, |ui| {
        for d in &view.stream.destinations {
            let live = view.live.iter().find(|s| s.id == d.id);
            ui.push_id(("destination", &d.id), |ui| row(ui, view, d, live, panel, acts, palette));
            ui.separator();
        }
    });
}

fn row(
    ui: &mut Ui,
    view: &View,
    d: &Destination,
    live: Option<&DestinationState>,
    panel: &mut StreamingPanel,
    acts: &mut Vec<Act>,
    palette: &Palette,
) {
    let title = if d.name.trim().is_empty() { d.id.clone() } else { d.name.clone() };
    ui.horizontal(|ui| {
        let mut on = d.enabled;
        if ui.checkbox(&mut on, RichText::new(title).strong()).on_hover_text(tl!("Send to this destination when the stream starts")).changed() {
            acts.push(Act::run("stream.destination_set", json!({"id": d.id, "enabled": on})));
        }
        ui.with_layout(crate::i18n::trailing(ui), |ui| {
            let editing = panel.editing.as_ref().is_some_and(|e| e.id == d.id);
            let can_remove = !(view.running && live.is_some_and(sending));
            let remove = ui.add_enabled(can_remove, egui::Button::new(tl!("Remove")));
            if remove.on_disabled_hover_text(tl!("Stop this destination before removing it")).clicked() {
                acts.push(Act::run("stream.destination_remove", json!({"id": d.id})));
            }
            if ui.selectable_label(editing, tl!("Edit")).clicked() {
                panel.editing = if editing { None } else { Some(Edit::of(d)) };
            }
            if let Some(s) = live.filter(|_| view.running) {
                if sending(s) {
                    if ui.button(tl!("Stop")).clicked() {
                        acts.push(Act::run("stream.destination_stop", json!({"id": d.id})));
                    }
                } else if ui.button(tl!("Start")).clicked() {
                    acts.push(Act::run("stream.destination_start", json!({"id": d.id})));
                }
            }
        });
    });
    let server = if d.server.trim().is_empty() { tl!("No server set").to_string() } else { shown_server(&d.server) };
    ui.monospace(server);
    ui.label(RichText::new(panel_key_source(&d.key)).small().weak());
    if let KeyRef::Held(name) = &d.key
        && !(view.held)(name)
    {
        ui.colored_label(palette.warn, RichText::new(tl!("No key is held for this session yet")).small());
    }
    let (text, tone) = status_text(d, view.running, live);
    let color = match tone {
        Tone::Plain => ui.visuals().text_color(),
        Tone::Good => palette.good,
        Tone::Warn => palette.warn,
    };
    ui.colored_label(color, text);
    if let Some(e) = panel.editing.as_mut().filter(|e| e.id == d.id) {
        let close = details(ui, e, view, acts);
        if close {
            panel.editing = None;
        }
    }
}

/// The open details of a destination; whether they closed.
fn details(ui: &mut Ui, e: &mut Edit, view: &View, acts: &mut Vec<Act>) -> bool {
    let mut close = false;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        egui::Grid::new("destination-details").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label(tl!("Name"));
            ui.add(egui::TextEdit::singleline(&mut e.name).char_limit(MAX_NAME).desired_width(260.0));
            ui.end_row();
            ui.label(tl!("Server"));
            // The address can hold a key, so the field hides its text until Show.
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut e.server)
                        .password(!e.show_server)
                        .hint_text(tl!("rtmp://live.example.com/app"))
                        .char_limit(MAX_SERVER)
                        .desired_width(260.0),
                );
                ui.toggle_value(&mut e.show_server, tl!("Show"));
            });
            ui.end_row();
            ui.label(tl!("Stream key"));
            let choices: Vec<_> = KeyChoice::ALL.iter().map(|&c| Entry::new(c, tl!(c.label()))).collect();
            let _ = Select::new(tl!("Stream key"), Bind::bound(&mut e.key), &choices).id_salt("key-choice").show(ui);
            ui.end_row();
            if e.key == KeyChoice::Held {
                ui.label(tl!("Session key name"));
                ui.add(egui::TextEdit::singleline(&mut e.held_name).char_limit(MAX_DESTINATION_ID).desired_width(160.0));
                ui.end_row();
                ui.label(tl!("Key for this session"));
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut e.held_key).password(true).desired_width(200.0));
                    let name = e.held_name.trim().to_string();
                    if ui.add_enabled(!name.is_empty() && !e.held_key.is_empty(), egui::Button::new(tl!("Hold"))).clicked() {
                        acts.push(Act::HoldStreamKey { name: name.clone(), key: std::mem::take(&mut e.held_key) });
                    }
                    if ui.add_enabled(!name.is_empty() && (view.held)(&name), egui::Button::new(tl!("Forget"))).clicked() {
                        acts.push(Act::HoldStreamKey { name, key: String::new() });
                    }
                });
                ui.end_row();
            }
        });
        ui.horizontal(|ui| {
            // The details close when the change is made, so a refused one keeps what was typed.
            if ui.button(tl!("Save")).clicked() {
                acts.push(Act::run("stream.destination_set", e.params()));
            }
            if ui.button(tl!("Cancel")).clicked() {
                close = true;
            }
        });
    });
    close
}

/// The `id` parameter.
fn param_id(cmd: &craft_keymap::CommandRef) -> Result<&str, Msg> {
    cmd.params.get("id").and_then(Value::as_str).ok_or_else(|| tmsg!("`{command}` needs `id`: a destination id", command = cmd.id))
}

/// An optional string parameter; an error when it is there and not a string.
fn opt_str<'a>(cmd: &'a craft_keymap::CommandRef, key: &str) -> Result<Option<&'a str>, Msg> {
    match cmd.params.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(tmsg!("`{command}`: `{key}` is a string", command = cmd.id, key = key)),
    }
}

/// The `key` and `held_name` parameters as a key source, or `None` when `key` is left out.
fn key_param(cmd: &craft_keymap::CommandRef, id: &str, current: Option<&KeyRef>) -> Result<Option<KeyRef>, Msg> {
    let held_name = opt_str(cmd, "held_name")?;
    let Some(key) = opt_str(cmd, "key")? else {
        if held_name.is_some() {
            return Err(tmsg!("`{command}`: `held_name` goes with `key`: `held`", command = cmd.id));
        }
        return Ok(None);
    };
    let key = match key {
        "none" => KeyRef::None,
        "profile" => KeyRef::Profile,
        "held" => {
            let name = match (held_name, current) {
                (Some(n), _) => n.trim().to_string(),
                (None, Some(KeyRef::Held(n))) => n.clone(),
                (None, _) => id.to_string(),
            };
            if name.is_empty() || name.len() > MAX_DESTINATION_ID {
                return Err(tmsg!("`{command}`: `held_name` is 1 to {max} bytes", command = cmd.id, max = MAX_DESTINATION_ID));
            }
            KeyRef::Held(name)
        }
        other => return Err(tmsg!("`{command}`: `key` is `profile`, `held` or `none`, not `{key}`", command = cmd.id, key = other)),
    };
    if held_name.is_some() && !matches!(key, KeyRef::Held(_)) {
        return Err(tmsg!("`{command}`: `held_name` goes with `key`: `held`", command = cmd.id));
    }
    Ok(Some(key))
}

/// The `name` parameter, trimmed, at most [`MAX_NAME`] characters.
fn name_param(cmd: &craft_keymap::CommandRef) -> Result<Option<String>, Msg> {
    let v = text_param(cmd, "name")?;
    if v.as_ref().is_some_and(|v| v.chars().count() > MAX_NAME) {
        return Err(tmsg!("`{command}`: `name` is longer than {max} characters", command = cmd.id, max = MAX_NAME));
    }
    Ok(v)
}

/// The `server` parameter, trimmed, at most [`MAX_SERVER`] bytes, without a user name or password
/// before the host, which the stream does not connect with either.
fn server_param(cmd: &craft_keymap::CommandRef) -> Result<Option<String>, Msg> {
    let v = text_param(cmd, "server")?;
    if v.as_ref().is_some_and(|v| v.len() > MAX_SERVER) {
        return Err(tmsg!("`{command}`: `server` is longer than {max} bytes", command = cmd.id, max = MAX_SERVER));
    }
    if v.as_deref().and_then(user_info).is_some() {
        return Err(tmsg!("`{command}`: `server` has a user name or password before the host, which is not supported", command = cmd.id));
    }
    Ok(v)
}

/// Refuses a change a command makes without the user's own action in the window when it would
/// send a stream key where the user did not send it: `key` `profile` for a destination other
/// than the primary one, a `held_name` the destination did not already use, or the profile's key
/// or a held key going with a server it did not go with before. `held_name` is the command's
/// `held_name` parameter. `before` is the destination's key and server before the change, `None`
/// for a new destination; `after` is the same after it.
fn guard_stream_key(
    command: &str,
    id: &str,
    key_given: Option<&KeyRef>,
    held_name: Option<&str>,
    before: Option<(&KeyRef, &str)>,
    after: (&KeyRef, &str),
) -> Result<(), Msg> {
    if key_given == Some(&KeyRef::Profile) && id != PRIMARY_DESTINATION {
        return Err(tmsg!(
            "`{command}`: only the destination `{primary}` takes `key`: `profile` from a command; choose the profile's key for `{id}` in the Streaming window",
            command = command,
            primary = PRIMARY_DESTINATION,
            id = id
        ));
    }
    if held_name.is_some()
        && let Some(KeyRef::Held(name)) = key_given
        && before.map(|(key, _)| key) != key_given
    {
        return Err(tmsg!(
            "`{command}`: `{id}` does not use the session key `{name}`; give `key`: `held` without `held_name`, or choose its session key in the Streaming window",
            command = command,
            id = id,
            name = name
        ));
    }
    let paired =
        |(key, server): (&KeyRef, &str)| (*key != KeyRef::None && !server.trim().is_empty()).then(|| (key.clone(), server.trim().to_string()));
    let now = paired(after);
    if now.is_none() || now == before.and_then(paired) {
        return Ok(());
    }
    if *after.0 == KeyRef::Profile {
        return Err(tmsg!(
            "`{command}`: `{id}` would send the profile's stream key to a new server; give `key`: `none` with the server, or make the change in the Streaming window",
            command = command,
            id = id
        ));
    }
    Err(tmsg!(
        "`{command}`: `{id}` would send its session key to a new server; give `key`: `none` with the server, or make the change in the Streaming window",
        command = command,
        id = id
    ))
}

/// An optional text parameter, trimmed; an error when it holds a control character.
fn text_param(cmd: &craft_keymap::CommandRef, key: &str) -> Result<Option<String>, Msg> {
    let Some(v) = opt_str(cmd, key)? else { return Ok(None) };
    if v.chars().any(char::is_control) {
        return Err(tmsg!("`{command}`: `{key}` has a control character", command = cmd.id, key = key));
    }
    Ok(Some(v.trim().to_string()))
}

/// An optional `enabled` parameter.
fn opt_bool(cmd: &craft_keymap::CommandRef, key: &str) -> Result<Option<bool>, Msg> {
    match cmd.params.get(key) {
        None => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(tmsg!("`{command}`: `{key}` is true or false", command = cmd.id, key = key)),
    }
}

/// An id no destination uses, and that the running stream (`live`) does not use either, so a new
/// destination is never started as one that was removed: `main` for the first one, otherwise
/// the first free `destination-N`.
fn new_id(s: &StreamSettings, live: &[DestinationState]) -> String {
    let free = |id: &str| s.destination(id).is_none() && !live.iter().any(|d| d.id == id);
    if s.destinations.is_empty() && free(PRIMARY_DESTINATION) {
        return PRIMARY_DESTINATION.to_string();
    }
    (2..=2 * MAX_DESTINATIONS + 1).map(|n| format!("destination-{n}")).find(|id| free(id)).unwrap_or_else(|| "destination".to_string())
}

/// A destination as `stream.destinations` lists it.
fn listed(d: &Destination, running: bool, live: Option<&DestinationState>, held: bool) -> Value {
    let mut v = json!({
        "id": d.id,
        "name": d.name,
        "enabled": d.enabled,
        "server": shown_server(&d.server),
        "key": KeyChoice::of(&d.key).name(),
    });
    if let KeyRef::Held(n) = &d.key {
        v["held_name"] = json!(n);
        v["held"] = json!(held);
    }
    let (status, error) = match (running, live.map(|s| &s.status)) {
        (true, Some(DestinationStatus::Connecting)) => ("connecting", None),
        (true, Some(DestinationStatus::Live)) => ("live", None),
        (true, Some(DestinationStatus::Retrying { error, .. })) => ("retrying", Some(error.as_str())),
        (true, Some(DestinationStatus::Stopped)) => ("stopped", None),
        (true, Some(DestinationStatus::Failed(e))) => ("failed", Some(e.as_str())),
        (true, None) => ("not_in_stream", None),
        (false, _) => ("idle", None),
    };
    v["status"] = json!(status);
    if let Some(e) = error {
        v["error"] = json!(e);
    }
    if let Some(s) = live.filter(|_| running) {
        v["bytes"] = json!(s.bytes);
        v["dropped"] = json!(s.dropped);
        v["reconnects"] = json!(s.reconnects);
        if let DestinationStatus::Retrying { attempt, .. } = s.status {
            v["attempt"] = json!(attempt);
        }
    }
    v
}

impl StudioApp {
    fn stream_running(&self) -> bool {
        self.studio.output(OutputKind::Streaming).is_some_and(|o| o.active)
    }

    /// The running stream's destinations when it started with the profile in use; empty for a
    /// stream of another profile, whose destinations are not this profile's.
    fn live_destinations(&self) -> Vec<DestinationState> {
        if self.studio.stream_profile() == Some(self.studio.profile().name.as_str()) { self.studio.stream_destinations() } else { Vec::new() }
    }

    /// Change the destination `touched` of the profile with `f`, check the destinations, put the
    /// profile in use and save it; whether anything changed. Nothing is saved when nothing changed,
    /// and a profile that cannot be saved is put back as it was. In an open Settings window's draft
    /// only `touched` takes the change, so the draft's unsaved edits of other destinations stay.
    fn change_destinations(&mut self, touched: &str, f: impl FnOnce(&mut StreamSettings) -> Result<(), Msg>) -> Result<bool, Msg> {
        let previous = self.studio.profile().clone();
        let mut p = previous.clone();
        f(&mut p.stream)?;
        p.stream.check().map_err(Msg::plain)?;
        if p.stream == previous.stream {
            return Ok(false);
        }
        self.studio.set_profile(p).map_err(|e| e.to_string())?;
        if let Err(e) = self.store.save_profile(self.studio.profile()) {
            let _ = self.studio.set_profile(previous);
            return Err(tmsg!("The profile could not be saved: {error}", error = e));
        }
        if let Some(w) = &mut self.settings {
            let draft = &mut w.draft.stream.destinations;
            let new = self.studio.profile().stream.destination(touched).cloned();
            match (draft.iter().position(|d| d.id == touched), new) {
                (Some(i), Some(d)) => draft[i] = d,
                (Some(i), None) => {
                    draft.remove(i);
                }
                (None, Some(d)) if draft.len() < MAX_DESTINATIONS => draft.push(d),
                (None, _) => {}
            }
        }
        Ok(true)
    }

    /// Close the window's open details when another profile is in use, so they are not saved
    /// into it.
    fn sync_streaming_profile(&mut self) {
        let name = &self.studio.profile().name;
        if let Some(panel) = &mut self.streaming
            && panel.profile != *name
        {
            panel.editing = None;
            panel.profile.clone_from(name);
        }
    }

    /// The window's `view.streaming` and `stream.destination*` commands.
    pub(crate) fn streaming_command(&mut self, cmd: &craft_keymap::CommandRef) -> Result<Value, Msg> {
        self.sync_streaming_profile();
        match cmd.id.as_str() {
            "view.streaming" => {
                self.streaming =
                    if self.streaming.is_some() { None } else { Some(StreamingPanel { editing: None, profile: self.studio.profile().name.clone() }) };
                Ok(json!({"on": self.streaming.is_some()}))
            }
            "stream.destinations" => {
                let running = self.stream_running();
                let live = self.live_destinations();
                let s = &self.studio.profile().stream;
                let list: Vec<Value> = s
                    .destinations
                    .iter()
                    .map(|d| {
                        let held = matches!(&d.key, KeyRef::Held(n) if self.studio.stream_key_held(n));
                        listed(d, running, live.iter().find(|l| l.id == d.id), held)
                    })
                    .collect();
                Ok(json!({"destinations": list, "running": running, "max": MAX_DESTINATIONS}))
            }
            "stream.destination_add" => {
                let name = name_param(cmd)?;
                let server = server_param(cmd)?;
                let enabled = opt_bool(cmd, "enabled")?;
                let live = self.live_destinations();
                let current = &self.studio.profile().stream;
                let id = new_id(current, &live);
                let first = current.destinations.is_empty();
                let key_given = key_param(cmd, &id, None)?;
                let key = key_given.clone().unwrap_or(if first { KeyRef::Profile } else { KeyRef::None });
                if !self.from_user {
                    let held_name = opt_str(cmd, "held_name")?;
                    guard_stream_key(&cmd.id, &id, key_given.as_ref(), held_name, None, (&key, server.as_deref().unwrap_or("")))?;
                }
                let d = Destination {
                    id: id.clone(),
                    name: name.unwrap_or_default(),
                    enabled: enabled.unwrap_or(true),
                    server: server.unwrap_or_default(),
                    key,
                };
                self.change_destinations(&id, |s| {
                    if s.destinations.len() >= MAX_DESTINATIONS {
                        return Err(tmsg!("a profile holds at most {max} stream destinations", max = MAX_DESTINATIONS));
                    }
                    s.destinations.push(d);
                    Ok(())
                })?;
                if let Some(panel) = &mut self.streaming
                    && let Some(d) = self.studio.profile().stream.destination(&id)
                {
                    panel.editing = Some(Edit::of(d));
                }
                Ok(json!({"id": id}))
            }
            "stream.destination_remove" => {
                let id = param_id(cmd)?.to_string();
                let in_use = self.stream_running() && self.live_destinations().iter().any(|s| s.id == id && sending(s));
                self.change_destinations(&id, |s| {
                    let i = s.destinations.iter().position(|d| d.id == id).ok_or_else(|| tmsg!("there is no stream destination `{id}`", id = id))?;
                    if in_use {
                        return Err(tmsg!("stop the stream destination `{id}` before removing it", id = id));
                    }
                    s.destinations.remove(i);
                    Ok(())
                })?;
                if let Some(panel) = &mut self.streaming
                    && panel.editing.as_ref().is_some_and(|e| e.id == id)
                {
                    panel.editing = None;
                }
                Ok(json!({"removed": id}))
            }
            "stream.destination_set" => {
                let id = param_id(cmd)?.to_string();
                let name = name_param(cmd)?;
                let server = server_param(cmd)?;
                let enabled = opt_bool(cmd, "enabled")?;
                let from_user = self.from_user;
                let changed = self.change_destinations(&id, |s| {
                    let d = s.destinations.iter_mut().find(|d| d.id == id).ok_or_else(|| tmsg!("there is no stream destination `{id}`", id = id))?;
                    let key = key_param(cmd, &id, Some(&d.key))?;
                    if !from_user {
                        let after = (key.as_ref().unwrap_or(&d.key), server.as_deref().unwrap_or(&d.server));
                        guard_stream_key(&cmd.id, &id, key.as_ref(), opt_str(cmd, "held_name")?, Some((&d.key, &d.server)), after)?;
                    }
                    if let Some(n) = &name {
                        d.name.clone_from(n);
                    }
                    if let Some(v) = &server {
                        d.server.clone_from(v);
                    }
                    if let Some(on) = enabled {
                        d.enabled = on;
                    }
                    if let Some(k) = key {
                        d.key = k;
                    }
                    Ok(())
                })?;
                if let Some(panel) = &mut self.streaming
                    && panel.editing.as_ref().is_some_and(|e| e.id == id)
                {
                    panel.editing = None;
                }
                Ok(json!({"id": id, "changed": changed}))
            }
            id @ ("stream.destination_start" | "stream.destination_stop") => {
                let dest = param_id(cmd)?.to_string();
                let on = id == "stream.destination_start";
                if self.stream_running() && self.studio.stream_profile() != Some(self.studio.profile().name.as_str()) {
                    return Err(tmsg!("the running stream was started with another profile"));
                }
                self.studio.set_stream_destination(&dest, on).map_err(|e| e.to_string())?;
                Ok(json!({"id": dest, "on": on}))
            }
            other => Err(tmsg!("unknown command `{command}`", command = other)),
        }
    }

    /// Draw the Streaming window while it is open.
    pub(crate) fn streaming_window(&mut self, ctx: &egui::Context, acts: &mut Vec<Act>) {
        if self.streaming.is_none() {
            return;
        }
        self.sync_streaming_profile();
        let running = self.stream_running();
        let live = self.live_destinations();
        let studio = &self.studio;
        let held = |name: &str| studio.stream_key_held(name);
        let view = View { stream: &studio.profile().stream, running, live: &live, held: &held };
        let mut open = true;
        if let Some(panel) = &mut self.streaming {
            egui::Window::new(tid!("view.streaming", "Streaming"))
                .id(egui::Id::unique("streaming"))
                .open(&mut open)
                .default_width(440.0)
                .show(ctx, |ui| show(ui, &view, panel, acts));
        }
        if !open {
            self.streaming = None;
        }
    }
}

#[cfg(test)]
mod tests;
