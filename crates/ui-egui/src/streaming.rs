//! The Streaming window (View › Streaming): every stream destination of the profile with its on or
//! off setting, the streaming service it is set up for with the service's logo where the service
//! allows it, its server with the key hidden, where its key comes from and, while the stream runs,
//! what it is doing. Each change runs one of the window's `stream.destination*` commands. Check
//! Connection runs `stream.diagnose` and shows what the check found under the destination. For a
//! service that publishes a list of its ingest servers, List Servers runs `stream.ingest_servers`
//! and the details then offer the servers of that list.

use std::time::{Duration, UNIX_EPOCH};

use egui::{RichText, Role, Sense, Ui, WidgetInfo, vec2};
use serde_json::{Value, json};
use studiocraft_model::profile::{MAX_DESTINATION_ID, MAX_DESTINATIONS, PRIMARY_DESTINATION};
use studiocraft_model::services::{self, SERVICES, Service};
use studiocraft_model::{Destination, KeyRef, ReconnectSettings, StreamSettings};
use studiocraft_plugin::{Cause, DestinationState, DestinationStatus, Failure};
use studiocraft_studio::OutputKind;
use studiocraft_studio::diagnose::{Check, CheckState, Diagnosis, Evidence, Outcome, Verdict};
use studiocraft_studio::ingests::{self, IngestServer, ListState};

use crate::i18n::Msg;
use crate::kit::tokens::Palette;
use crate::logos::{self, Logos, ROW_TILE};
use crate::settings::key_source;
use crate::theme;
use crate::{Act, StudioApp};

/// Longest destination name, in characters.
pub const MAX_NAME: usize = 128;
/// Longest server address, in bytes.
pub const MAX_SERVER: usize = 4096;
/// How often the window is drawn again while a connection check runs.
const CHECK_REPAINT: Duration = Duration::from_millis(250);

/// What the Streaming window keeps between frames: the destination whose details are open, with
/// the text typed into them.
#[derive(Default)]
pub(crate) struct StreamingPanel {
    pub(crate) editing: Option<Edit>,
    /// The name of the profile the open details belong to.
    pub(crate) profile: String,
    /// The decoded service logos.
    pub(crate) logos: Logos,
}

/// The open details of one destination. Nothing changes until Save.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Edit {
    pub(crate) id: String,
    /// The id of the streaming service the destination is set up for; empty for a custom server.
    pub(crate) service: String,
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
        Edit {
            id: d.id.clone(),
            service: d.service.clone(),
            name: d.name.clone(),
            server: d.server.clone(),
            key: KeyChoice::of(&d.key),
            held_name,
            ..Edit::default()
        }
    }

    /// The `stream.destination_set` parameters that save these details.
    pub(crate) fn params(&self) -> Value {
        let mut p = json!({"id": self.id, "service": self.service, "name": self.name, "server": self.server, "key": self.key.name()});
        if self.key == KeyChoice::Held {
            p["held_name"] = json!(self.held_name);
        }
        p
    }

    /// Choose the service `id` (empty for a custom server), changing the server as a change of
    /// service does on save ([`services::set_service`]).
    fn set_service(&mut self, id: &str) {
        let mut d = Destination { server: std::mem::take(&mut self.server), service: self.service.clone(), ..Destination::default() };
        services::set_service(&mut d, id);
        self.server = d.server;
        self.service = d.service;
    }
}

/// The name of the service a destination was set up for, or Custom server.
fn service_name(id: &str) -> String {
    services::service(id).map_or_else(|| tl!("Custom server").to_string(), |svc| svc.name.to_string())
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
    match (running, live) {
        (true, Some(s)) => match &s.status {
            DestinationStatus::Connecting => (tl!("Connecting").to_string(), Tone::Plain),
            DestinationStatus::Live => (tl!("Live").to_string(), Tone::Good),
            DestinationStatus::Retrying { attempt, failure } => (
                craft_i18n::fmt(
                    tl!("Trying again (try {attempt}): {error}"),
                    &[("attempt", &attempt.to_string()), ("error", cause_text(failure.cause))],
                ),
                Tone::Warn,
            ),
            DestinationStatus::WaitingForNetwork { attempt, failure } => (
                craft_i18n::fmt(
                    tl!("Waiting for the network (try {attempt}): {error}"),
                    &[("attempt", &attempt.to_string()), ("error", cause_text(failure.cause))],
                ),
                Tone::Warn,
            ),
            DestinationStatus::Stopped => (tl!("Stopped").to_string(), Tone::Plain),
            DestinationStatus::Failed(failure) => (craft_i18n::fmt(tl!("Failed: {error}"), &[("error", cause_text(failure.cause))]), Tone::Warn),
        },
        (true, None) => (tl!("Not in this stream; it is sent to from the next start").to_string(), Tone::Plain),
        (false, _) if d.enabled => (tl!("Sent to when the stream starts").to_string(), Tone::Plain),
        (false, _) => (tl!("Off").to_string(), Tone::Plain),
    }
}

/// What kind of failure `cause` is, in the interface language.
pub(crate) fn cause_text(cause: Cause) -> &'static str {
    match cause {
        Cause::NoRoute => tl!("this computer has no network route to the server addresses tried"),
        Cause::LookupFailed => tl!("the server's name could not be looked up"),
        Cause::LookupNotFound => tl!("the server's name does not exist; check the server address"),
        Cause::LookupNoAnswer => tl!("the DNS server did not answer for the server's name"),
        Cause::LookupsPending => tl!("earlier lookups of server names have not finished"),
        Cause::Refused => tl!("the server refused the connection"),
        Cause::TimedOut => tl!("the server did not answer in time"),
        Cause::CertificateUntrusted => tl!("the server's certificate is not from a trusted authority"),
        Cause::CertificateNameMismatch => tl!("the server's certificate is for another name"),
        Cause::CertificateExpired => tl!("the server's certificate has expired"),
        Cause::CertificateNotAccepted => tl!("the server's certificate was not accepted"),
        Cause::TlsFailed => tl!("the secure connection failed"),
        Cause::ClosedDuringSetup => tl!("the server closed the connection before the stream started"),
        Cause::SetupTimedOut => tl!("the server did not finish setting up the stream in time"),
        Cause::RejectedConnect => tl!("the server refused the connection request; check the server address"),
        Cause::RejectedPublish => tl!("the server refused the stream; check the stream key"),
        Cause::Lost => tl!("the connection was lost"),
        Cause::BadAddress => tl!("the server address is not valid"),
        Cause::BadKey => tl!("the stream key is not valid"),
        Cause::Other => tl!("the connection failed"),
    }
}

/// The error behind the status of `live` while the stream runs, as the connection reported it,
/// with each address tried and its error on a line of its own.
pub(crate) fn status_detail(running: bool, live: Option<&DestinationState>) -> Option<String> {
    let failure = match live.filter(|_| running).map(|s| &s.status) {
        Some(
            DestinationStatus::Retrying { failure, .. } | DestinationStatus::WaitingForNetwork { failure, .. } | DestinationStatus::Failed(failure),
        ) => failure,
        _ => return None,
    };
    let mut text = Msg::plain(failure.detail.as_str()).shown();
    for a in &failure.addresses {
        text.push('\n');
        let cause = match a.cause {
            Cause::NoRoute => tl!("this computer has no network route to this address"),
            c => cause_text(c),
        };
        let args = [("address", a.address.as_str()), ("cause", cause), ("error", a.detail.as_str())];
        text.push_str(&craft_i18n::fmt(tl!("{address}: {cause} ({error})"), &args));
    }
    Some(text)
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
    /// The connection check of each destination that has had one in this session, by id.
    pub(crate) checks: &'a [(String, CheckState)],
    /// The list of ingest servers of each service whose list has been asked for, by service id.
    pub(crate) ingests: &'a [(&'static str, ListState)],
}

/// What a check of a destination found, in the interface language.
pub(crate) fn verdict_msg(d: &Diagnosis) -> Msg {
    let (template, mut args) = d.message();
    if let Some(c) = d.reason {
        for (k, v) in &mut args {
            if *k == "reason" {
                *v = cause_text(c).to_string();
            }
        }
    }
    Msg::new(template, args)
}

/// What one evidence line checked, in the interface language.
pub(crate) fn check_text(c: Check) -> &'static str {
    match c {
        Check::DefaultRoute => tl!("Route to any address"),
        Check::Route => tl!("Route to"),
        Check::Proxy => tl!("Proxy variable"),
        Check::Lookup => tl!("Name lookup of"),
        Check::Connect => tl!("Connection to"),
        Check::SecureConnect => tl!("Secure connection to"),
        Check::Connect443 => tl!("Secure connection on port 443 to"),
        Check::IngestList => tl!("The service's list of ingest servers"),
        Check::OtherIngest => tl!("Secure connection to the service's ingest server"),
        Check::StatusPage => tl!("The service's status summary"),
    }
}

/// How one evidence line came out, in the interface language.
pub(crate) fn outcome_text(o: Outcome) -> &'static str {
    match o {
        Outcome::Ok => tl!("works"),
        Outcome::Failed(c) => cause_text(c),
        Outcome::Set => tl!("set"),
        Outcome::Problem => tl!("reports a problem"),
        Outcome::Unknown => tl!("this computer could not say"),
        Outcome::Unfinished => tl!("not finished in time"),
    }
}

/// One evidence line of a check, in the interface language.
pub(crate) fn evidence_text(e: &Evidence) -> String {
    let args = [("check", check_text(e.check)), ("target", e.target.as_str()), ("outcome", outcome_text(e.outcome)), ("detail", e.detail.as_str())];
    let template = match (e.target.is_empty(), e.detail.is_empty()) {
        (true, true) => tl!("{check}: {outcome}"),
        (true, false) => tl!("{check}: {outcome} ({detail})"),
        (false, true) => tl!("{check} {target}: {outcome}"),
        (false, false) => tl!("{check} {target}: {outcome} ({detail})"),
    };
    craft_i18n::fmt(template, &args)
}

/// A check as `stream.destinations` lists it.
fn listed_check(state: &CheckState) -> Value {
    match state {
        CheckState::Running { automatic } => json!({"state": "running", "automatic": automatic}),
        CheckState::Done(d) => {
            let (template, args) = d.message();
            let args: serde_json::Map<String, Value> = args.into_iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
            let evidence: Vec<Value> = d
                .evidence
                .iter()
                .map(|e| json!({"check": e.check.id(), "target": e.target, "outcome": e.outcome.id(), "detail": e.detail}))
                .collect();
            let at_ms = d.at.duration_since(UNIX_EPOCH).map_or(0, |t| u64::try_from(t.as_millis()).unwrap_or(u64::MAX));
            json!({
                "state": "done",
                "automatic": d.automatic,
                "verdict": d.verdict.id(),
                "message": d.english(),
                "template": template,
                "args": args,
                "evidence": evidence,
                "at_ms": at_ms,
            })
        }
    }
}

/// The Streaming window's contents.
pub(crate) fn show(ui: &mut Ui, view: &View, panel: &mut StreamingPanel, acts: &mut Vec<Act>) {
    let palette = theme::palette(ui.ctx());
    let count = view.stream.destinations.len();
    ui.horizontal(|ui| {
        let full = count >= MAX_DESTINATIONS;
        // A destination for a streaming service, or for a server of the user's own choosing.
        ui.add_enabled_ui(!full, |ui| {
            ui.menu_button(tl!("Add Destination"), |ui| {
                for svc in SERVICES {
                    if ui.button(svc.name).clicked() {
                        acts.push(Act::run("stream.destination_add", json!({"service": svc.id})));
                        ui.close();
                    }
                }
                ui.separator();
                if ui.button(tl!("Custom server")).clicked() {
                    acts.push(Act::run("stream.destination_add", json!({})));
                    ui.close();
                }
            });
        });
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
    let preset = services::service(&d.service);
    let title = match preset {
        _ if !d.name.trim().is_empty() => d.name.clone(),
        Some(svc) => svc.name.to_string(),
        None => d.id.clone(),
    };
    ui.horizontal(|ui| {
        // A destination set up for a service shows the service's logo where it may stand beside
        // text at this size, and a plain tile otherwise.
        if preset.is_some() {
            let (tile, _) = ui.allocate_exact_size(vec2(ROW_TILE, ROW_TILE), Sense::hover());
            if ui.is_rect_visible(tile) {
                let radius = theme::sizes(ui.ctx()).radius_control;
                match logos::logo(&d.service).filter(|l| l.fits_row(ROW_TILE, ui.ctx().pixels_per_point())) {
                    Some(l) => panel.logos.paint(ui, tile, l, palette, radius),
                    None => Logos::paint_plain(ui, tile, palette, radius),
                }
            }
        }
        let mut on = d.enabled;
        if ui.checkbox(&mut on, RichText::new(title).strong()).on_hover_text(tl!("Send to this destination when the stream starts")).changed() {
            acts.push(Act::run("stream.destination_set", json!({"id": d.id, "enabled": on})));
        }
        ui.with_layout(craft_ui::direction::trailing(ui), |ui| {
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
    if let Some(svc) = preset {
        ui.label(RichText::new(craft_i18n::fmt(tl!("Stream to {service}"), &[("service", svc.name)])).small());
    }
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
    let status = ui.colored_label(color, text);
    if let Some(detail) = status_detail(view.running, live) {
        status.on_hover_text(detail);
    }
    check_row(ui, view, d, acts, palette);
    let StreamingPanel { editing, logos, .. } = panel;
    if let Some(e) = editing.as_mut().filter(|e| e.id == d.id) {
        let close = details(ui, e, view, logos, acts);
        if close {
            *editing = None;
        }
    }
}

/// Check Connection for destination `d`, and what its last check found, with the evidence in a
/// list that opens.
fn check_row(ui: &mut Ui, view: &View, d: &Destination, acts: &mut Vec<Act>, palette: &Palette) {
    let check = view.checks.iter().find(|(id, _)| *id == d.id).map(|(_, c)| c);
    let running = matches!(check, Some(CheckState::Running { .. }));
    ui.horizontal(|ui| {
        let has_server = !d.server.trim().is_empty();
        let button = ui.add_enabled(has_server && !running, egui::Button::new(tl!("Check Connection")).small());
        let button = button.on_hover_text(tl!(
            "Asks this computer whether it has a route to the server, looks the server's name up, and connects to the server on its \
             port and on port 443, with a secure connection where the address uses one. For a service that publishes them, it also \
             reads the service's list of ingest servers and status summary and connects to two of its other ingest servers. It sends \
             no stream key and nothing of the address after the server's name. Each server it contacts sees this computer's address, \
             and a secure connection sends the server's name unencrypted."
        ));
        let button = if has_server { button } else { button.on_disabled_hover_text(tl!("Set a server for this destination first")) };
        if button.clicked() {
            acts.push(Act::run("stream.diagnose", json!({"id": d.id})));
        }
        if running {
            ui.spinner();
            ui.weak(tl!("Checking the connection…"));
        }
    });
    let Some(CheckState::Done(diag)) = check else { return };
    let color = if diag.verdict == Verdict::Reachable { palette.good } else { palette.warn };
    ui.colored_label(color, verdict_msg(diag).shown());
    if diag.automatic {
        ui.label(RichText::new(tl!("Checked by itself after the destination failed")).small().weak());
    }
    egui::CollapsingHeader::new(RichText::new(tl!("What the check found")).small()).id_salt(("check-evidence", &d.id)).show(ui, |ui| {
        for e in &diag.evidence {
            ui.label(RichText::new(evidence_text(e)).small());
        }
    });
}

/// The logo of the service `e` is set up for, on a row of its own at its full size, linked to the
/// service's page about streaming.
fn details_logo(ui: &mut Ui, e: &Edit, logos: &mut Logos, acts: &mut Vec<Act>) {
    let (Some(svc), Some(l)) = (services::service(&e.service), logos::logo(&e.service)) else { return };
    let tile = l.header_tile(ui.ctx().pixels_per_point());
    let (rect, r) = ui.allocate_exact_size(vec2(l.header_width(tile), tile), Sense::click());
    let hint = craft_i18n::fmt(tl!("Where {service} shows your server and key"), &[("service", svc.name)]);
    r.widget_info(|| WidgetInfo::labeled(Role::Link, true, &hint));
    if ui.is_rect_visible(rect) {
        let p = theme::palette(ui.ctx());
        logos.paint(ui, rect, l, p, theme::sizes(ui.ctx()).radius_surface);
    }
    if r.on_hover_text(&hint).clicked() {
        acts.push(Act::OpenUrl(svc.source.to_string()));
    }
    ui.add_space(l.header_gap(tile));
}

/// The servers the details offer for `svc` from its list `list`: the service's documented server
/// first, named Default, when the list does not hold it, then the list's servers in its order.
fn ingest_choices<'a>(svc: &'static Service, list: &'a [IngestServer]) -> impl Iterator<Item = (&'a str, &'a str)> {
    let default = svc.server.filter(|server| !list.iter().any(|s| s.server == *server)).map(|server| (tl!("Default"), server));
    default.into_iter().chain(list.iter().map(|s| (s.name.as_str(), s.server.as_str())))
}

/// The choice of one of the ingest servers `svc` publishes, which sets the server of `e`; a typed
/// server is kept until a server of the list is chosen. Until the list is read, List Servers asks
/// for it.
fn ingest_choice(ui: &mut Ui, e: &mut Edit, svc: &'static Service, state: Option<&ListState>, acts: &mut Vec<Act>) {
    let list_button = |ui: &mut Ui, acts: &mut Vec<Act>| {
        let button = ui.button(tl!("List Servers")).on_hover_text(tl!(
            "Reads the service's list of its ingest servers from the service's site, which sees this computer's address. It sends no stream key."
        ));
        if button.clicked() {
            acts.push(Act::run("stream.ingest_servers", json!({"service": svc.id, "retry": true})));
        }
    };
    match state {
        Some(ListState::Ready(list)) => {
            let current = e.server.trim();
            let chosen = ingest_choices(svc, list).find(|(_, server)| *server == current).map(|(name, _)| name.to_string());
            let typed = if current.is_empty() { tl!("Choose a server") } else { tl!("Typed server") };
            let shown = chosen.unwrap_or_else(|| typed.to_string());
            let mut pick = None;
            craft_ui::ComboBox::from_id_salt("destination-ingest").selected_text(shown).width(260.0).show_ui(ui, |ui| {
                for (name, server) in ingest_choices(svc, list) {
                    if ui.selectable_label(server == current, name).on_hover_text(server).clicked() {
                        pick = Some(server.to_string());
                    }
                }
            });
            if let Some(server) = pick {
                e.server = server;
            }
        }
        Some(ListState::Loading) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(tl!("Reading the list of servers…"));
            });
        }
        Some(ListState::Failed(error)) => {
            ui.vertical(|ui| {
                list_button(ui, acts);
                let text = craft_i18n::fmt(tl!("The list could not be read: {error}"), &[("error", error)]);
                ui.colored_label(theme::palette(ui.ctx()).warn, RichText::new(text).small());
            });
        }
        None => list_button(ui, acts),
    }
}

/// A service's list of ingest servers as `stream.ingest_servers` returns it.
fn listed_ingests(svc: &Service, state: &ListState) -> Value {
    match state {
        ListState::Loading => json!({"service": svc.id, "state": "loading"}),
        ListState::Ready(list) => {
            let servers: Vec<Value> = list.iter().map(|s| json!({"name": s.name, "server": s.server})).collect();
            json!({"service": svc.id, "state": "ready", "servers": servers})
        }
        ListState::Failed(error) => json!({"service": svc.id, "state": "failed", "error": error}),
    }
}

/// The open details of a destination; whether they closed.
fn details(ui: &mut Ui, e: &mut Edit, view: &View, logos: &mut Logos, acts: &mut Vec<Act>) -> bool {
    let mut close = false;
    egui::Frame::group(ui.style()).show(ui, |ui| {
        details_logo(ui, e, logos, acts);
        egui::Grid::new("destination-details").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
            ui.label(tl!("Service"));
            craft_ui::ComboBox::from_id_salt("destination-service").selected_text(service_name(&e.service)).width(260.0).show_ui(ui, |ui| {
                if ui.selectable_label(e.service.is_empty(), tl!("Custom server")).clicked() && !e.service.is_empty() {
                    e.set_service("");
                }
                for svc in SERVICES {
                    if ui.selectable_label(e.service == svc.id, svc.name).clicked() && e.service != svc.id {
                        e.set_service(svc.id);
                    }
                }
            });
            ui.end_row();
            ui.label(tl!("Name"));
            let preset = services::service(&e.service);
            let name_hint = preset.map_or("", |svc| svc.name);
            ui.add(egui::TextEdit::singleline(&mut e.name).char_limit(MAX_NAME).hint_text(name_hint).desired_width(260.0));
            ui.end_row();
            ui.label(tl!("Server"));
            // A service that shows each account its own server says where to find it.
            let own_server = preset.filter(|svc| svc.server.is_none());
            let server_hint = if own_server.is_some() { tl!("Copy it from your account") } else { tl!("rtmp://live.example.com/app") };
            // The address can hold a key, so the field hides its text until Show.
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut e.server)
                            .password(!e.show_server)
                            .hint_text(server_hint)
                            .char_limit(MAX_SERVER)
                            .desired_width(260.0),
                    );
                    ui.toggle_value(&mut e.show_server, tl!("Show"));
                });
                if let Some(svc) = own_server {
                    ui.horizontal(|ui| {
                        let text = craft_i18n::fmt(tl!("{service} shows the server and key for your account."), &[("service", svc.name)]);
                        ui.label(RichText::new(text).small().weak());
                        if ui.link(tl!("Where to find them")).clicked() {
                            acts.push(Act::OpenUrl(svc.source.to_string()));
                        }
                    });
                }
            });
            ui.end_row();
            // A service that publishes a list of its ingest servers offers them once it is read.
            if let Some(svc) = preset.filter(|svc| svc.ingests.is_some()) {
                ui.label(tl!("Ingest server"));
                let state = view.ingests.iter().find(|(id, _)| *id == svc.id).map(|(_, s)| s);
                ingest_choice(ui, e, svc, state, acts);
                ui.end_row();
            }
            ui.label(tl!("Stream key"));
            craft_ui::ComboBox::from_id_salt("key-choice").selected_text(tl!(e.key.label())).show_ui(ui, |ui| {
                for c in KeyChoice::ALL {
                    ui.selectable_value(&mut e.key, c, tl!(c.label()));
                }
            });
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

/// The `service` parameter: the id of a streaming service in [`SERVICES`], or empty for a server
/// of the user's own choosing. `current` is the destination's service, which is accepted as it is
/// even when this version does not know it.
fn service_param(cmd: &craft_keymap::CommandRef, current: Option<&str>) -> Result<Option<String>, Msg> {
    let Some(id) = opt_str(cmd, "service")? else { return Ok(None) };
    if !id.is_empty() && services::service(id).is_none() && current != Some(id) {
        let ids: Vec<&str> = SERVICES.iter().map(|s| s.id).collect();
        return Err(tmsg!("`{command}`: `service` is empty or one of {ids}, not `{service}`", command = cmd.id, ids = ids.join(", "), service = id));
    }
    Ok(Some(id.to_string()))
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

/// An optional whole-number parameter.
fn opt_u32(cmd: &craft_keymap::CommandRef, key: &str) -> Result<Option<u32>, Msg> {
    match cmd.params.get(key) {
        None => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| tmsg!("`{command}`: `{key}` is a whole number", command = cmd.id, key = key)),
    }
}

/// Reconnect settings as `stream.destinations` lists them.
fn listed_reconnect(r: &ReconnectSettings) -> Value {
    json!({"enabled": r.enabled, "first_delay_s": r.first_delay_s, "max_delay_s": r.max_delay_s, "max_tries": r.max_tries})
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
        "service": d.service,
        "server": shown_server(&d.server),
        "key": KeyChoice::of(&d.key).name(),
    });
    if let KeyRef::Held(n) = &d.key {
        v["held_name"] = json!(n);
        v["held"] = json!(held);
    }
    let (status, failure): (_, Option<&Failure>) = match (running, live.map(|s| &s.status)) {
        (true, Some(DestinationStatus::Connecting)) => ("connecting", None),
        (true, Some(DestinationStatus::Live)) => ("live", None),
        (true, Some(DestinationStatus::Retrying { failure, .. })) => ("retrying", Some(failure)),
        (true, Some(DestinationStatus::WaitingForNetwork { failure, .. })) => ("waiting_for_network", Some(failure)),
        (true, Some(DestinationStatus::Stopped)) => ("stopped", None),
        (true, Some(DestinationStatus::Failed(f))) => ("failed", Some(f)),
        (true, None) => ("not_in_stream", None),
        (false, _) => ("idle", None),
    };
    v["status"] = json!(status);
    if let Some(f) = failure {
        v["error"] = json!(f.detail);
        v["cause"] = json!(f.cause.id());
        if !f.addresses.is_empty() {
            let each: Vec<Value> = f.addresses.iter().map(|a| json!({"address": a.address, "cause": a.cause.id(), "error": a.detail})).collect();
            v["addresses"] = json!(each);
        }
    }
    if let Some(s) = live.filter(|_| running) {
        v["bytes"] = json!(s.bytes);
        v["dropped"] = json!(s.dropped);
        v["reconnects"] = json!(s.reconnects);
        if let DestinationStatus::Retrying { attempt, .. } | DestinationStatus::WaitingForNetwork { attempt, .. } = s.status {
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
            let _ = self.studio.restore_profile(previous);
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

    /// Changes the profile's reconnect settings with `f` and saves the profile. Settings outside
    /// their ranges are an error and change nothing; settings that end as they were are not saved.
    /// Returns whether they changed. A running stream keeps the settings it started with. An open
    /// Settings window takes the same change in its draft, keeping its other unsaved edits, unless
    /// the draft would then be out of range, when it takes the saved settings.
    pub(crate) fn change_reconnect(&mut self, f: impl Fn(&mut ReconnectSettings)) -> Result<bool, Msg> {
        let previous = self.studio.profile().clone();
        let mut p = previous.clone();
        f(&mut p.stream.reconnect);
        p.stream.reconnect.check().map_err(Msg::plain)?;
        if p.stream.reconnect == previous.stream.reconnect {
            return Ok(false);
        }
        self.studio.set_profile(p).map_err(|e| e.to_string())?;
        if let Err(e) = self.store.save_profile(self.studio.profile()) {
            let _ = self.studio.restore_profile(previous);
            return Err(tmsg!("The profile could not be saved: {error}", error = e));
        }
        if let Some(w) = &mut self.settings {
            let mut draft = w.draft.stream.reconnect;
            f(&mut draft);
            w.draft.stream.reconnect = if draft.check().is_ok() { draft } else { self.studio.profile().stream.reconnect };
        }
        Ok(true)
    }

    /// Turns the profile's `check_service` setting on or off and saves the profile; whether it
    /// changed. A call that changes nothing does not save the profile. An open Settings window
    /// takes the same change in its draft. A running stream keeps the setting it started with.
    fn change_check_service(&mut self, on: bool) -> Result<bool, Msg> {
        let previous = self.studio.profile().clone();
        if previous.stream.check_service == on {
            return Ok(false);
        }
        let mut p = previous.clone();
        p.stream.check_service = on;
        self.studio.set_profile(p).map_err(|e| e.to_string())?;
        if let Err(e) = self.store.save_profile(self.studio.profile()) {
            let _ = self.studio.restore_profile(previous);
            return Err(tmsg!("The profile could not be saved: {error}", error = e));
        }
        if let Some(w) = &mut self.settings {
            w.draft.stream.check_service = on;
        }
        Ok(true)
    }

    /// The connection checks of the profile's destinations that have had one in this session.
    fn destination_checks(&self) -> Vec<(String, CheckState)> {
        let s = &self.studio.profile().stream;
        s.destinations.iter().filter_map(|d| self.studio.stream_destination_check(&d.id).map(|c| (d.id.clone(), c))).collect()
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
                self.streaming = if self.streaming.is_some() {
                    None
                } else {
                    Some(StreamingPanel { profile: self.studio.profile().name.clone(), ..StreamingPanel::default() })
                };
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
                        let mut v = listed(d, running, live.iter().find(|l| l.id == d.id), held);
                        if let Some(c) = self.studio.stream_destination_check(&d.id) {
                            v["diagnosis"] = listed_check(&c);
                        }
                        v
                    })
                    .collect();
                let mut out = json!({
                    "destinations": list,
                    "running": running,
                    "max": MAX_DESTINATIONS,
                    "reconnect": listed_reconnect(&s.reconnect),
                    "check_service": s.check_service,
                });
                // Only a stream of this profile runs with this profile's settings.
                let own = self.studio.stream_profile() == Some(self.studio.profile().name.as_str());
                if let Some(r) = self.studio.stream_reconnect().filter(|_| running && own) {
                    out["running_reconnect"] = listed_reconnect(&r);
                }
                Ok(out)
            }
            "stream.reconnect_set" => {
                let enabled = opt_bool(cmd, "enabled")?;
                let first = opt_u32(cmd, "first_delay_s")?;
                let longest = opt_u32(cmd, "max_delay_s")?;
                let tries = opt_u32(cmd, "max_tries")?;
                let changed = self.change_reconnect(|r| {
                    r.enabled = enabled.unwrap_or(r.enabled);
                    r.first_delay_s = first.unwrap_or(r.first_delay_s);
                    r.max_delay_s = longest.unwrap_or(r.max_delay_s);
                    r.max_tries = tries.unwrap_or(r.max_tries);
                })?;
                Ok(json!({"changed": changed, "reconnect": listed_reconnect(&self.studio.profile().stream.reconnect)}))
            }
            "stream.check_service_set" => {
                let on = opt_bool(cmd, "enabled")?.ok_or_else(|| tmsg!("`{command}` needs `{key}`", command = cmd.id, key = "enabled"))?;
                let changed = self.change_check_service(on)?;
                Ok(json!({"changed": changed, "check_service": self.studio.profile().stream.check_service}))
            }
            "stream.diagnose" => {
                let id = param_id(cmd)?.to_string();
                let Some(d) = self.studio.profile().stream.destination(&id) else {
                    return Err(tmsg!("there is no stream destination `{id}`", id = id));
                };
                if d.server.trim().is_empty() {
                    return Err(tmsg!("the stream destination `{id}` has no server to check", id = id));
                }
                self.studio.check_stream_destination(&id).map_err(|e| e.to_string())?;
                Ok(json!({"id": id, "started": true}))
            }
            "stream.ingest_servers" => {
                let id = opt_str(cmd, "service")?.ok_or_else(|| tmsg!("`{command}` needs `{key}`", command = cmd.id, key = "service"))?;
                let Some(svc) = services::service(id).filter(|svc| svc.ingests.is_some()) else {
                    let ids: Vec<&str> = SERVICES.iter().filter(|svc| svc.ingests.is_some()).map(|svc| svc.id).collect();
                    return Err(tmsg!(
                        "`{command}`: `service` is a service that publishes a list of its ingest servers ({ids}), not `{service}`",
                        command = cmd.id,
                        ids = ids.join(", "),
                        service = id
                    ));
                };
                let retry = opt_bool(cmd, "retry")?.unwrap_or(false);
                let state = ingests::lists().request(svc, retry, || {}).map_err(Msg::plain)?;
                Ok(listed_ingests(svc, &state))
            }
            "stream.destination_add" => {
                let name = name_param(cmd)?;
                let service = service_param(cmd, None)?.unwrap_or_default();
                // A service that documents one ingest server for every account fills it in.
                let preset_server = services::service(&service).and_then(|svc| svc.server).map(str::to_string);
                let server = server_param(cmd)?.or(preset_server);
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
                    service,
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
                    let mut next = d.clone();
                    // A change of service changes the server as the service's preset does; a
                    // `server` or `enabled` given with it wins, and a destination given a server
                    // keeps its on or off setting.
                    if let Some(svc) = service_param(cmd, Some(&d.service))?
                        && svc != d.service
                    {
                        services::set_service(&mut next, &svc);
                    }
                    if let Some(n) = &name {
                        next.name.clone_from(n);
                    }
                    if let Some(v) = &server {
                        next.server.clone_from(v);
                        if !v.is_empty() {
                            next.enabled = d.enabled;
                        }
                    }
                    if let Some(on) = enabled {
                        next.enabled = on;
                    }
                    if let Some(k) = key.clone() {
                        next.key = k;
                    }
                    if !from_user {
                        guard_stream_key(
                            &cmd.id,
                            &id,
                            key.as_ref(),
                            opt_str(cmd, "held_name")?,
                            Some((&d.key, &d.server)),
                            (&next.key, &next.server),
                        )?;
                    }
                    *d = next;
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
        let checks = self.destination_checks();
        // A check reports from its own thread, and so does the fetch of a list of ingest servers,
        // so the window looks again while one runs.
        let lists = ingests::lists();
        let ingests: Vec<(&'static str, ListState)> =
            SERVICES.iter().filter(|svc| svc.ingests.is_some()).filter_map(|svc| lists.state(svc.id).map(|s| (svc.id, s))).collect();
        if checks.iter().any(|(_, c)| matches!(c, CheckState::Running { .. })) || ingests.iter().any(|(_, s)| *s == ListState::Loading) {
            ctx.request_repaint_after(CHECK_REPAINT);
        }
        let studio = &self.studio;
        let held = |name: &str| studio.stream_key_held(name);
        let view = View { stream: &studio.profile().stream, running, live: &live, held: &held, checks: &checks, ingests: &ingests };
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
