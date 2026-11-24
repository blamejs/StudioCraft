//! The Streaming window (View › Streaming): every stream destination of the profile with its on or
//! off setting, the streaming service it is set up for with the service's logo where the service
//! allows it, its server with the key hidden, where its key comes from and, while the stream runs,
//! what it is doing. A row of service tiles adds a destination. Each change runs one of the
//! window's `stream.destination*` commands. Check Connection runs `stream.diagnose` and shows
//! what the check found under the destination. Go Live to N, at the top, runs `golive.check`,
//! lists each problem with a button for its fix, and runs `stream.start` when there is none.

use std::time::{Duration, Instant, UNIX_EPOCH};

use craft_theme::variant::Tone;
use craft_theme::{ControlSize, Variant};
use craft_ui::buttons::{Button, Link, ToggleButton};
use craft_ui::choice::Checkbox;
use craft_ui::contract::{Bind, Entry, Props as _, Show as _, ShowContainer as _, Sizing, TooltipSpec};
use craft_ui::dialog::Dialog;
use craft_ui::display::{Divider, Elide, Shade, Spinner, Text};
use craft_ui::form::{Field, FieldGroup};
use craft_ui::layout_components::{Row, Stack};
use craft_ui::pickers::Select;
use craft_ui::sections::{GroupBox, Section};
use craft_ui::style::TypeRole;
use craft_ui::text::TextField;
use egui::{Role, Sense, Ui, WidgetInfo, vec2};
use serde_json::{Value, json};
use studiocraft_model::profile::{MAX_DESTINATION_ID, MAX_DESTINATIONS, PRIMARY_DESTINATION};
use studiocraft_model::services::{self, SERVICES};
use studiocraft_model::{Destination, KeyRef, ReconnectSettings, StreamSettings};
use studiocraft_plugin::{Cause, DestinationState, DestinationStatus, Failure, OutputStatus};
use studiocraft_studio::OutputKind;
use studiocraft_studio::diagnose::{Check, CheckState, Diagnosis, Evidence, Outcome, Verdict};

use crate::i18n::Msg;
use crate::kit::option_card::{OptionCard, OptionCardStyle, OptionCards};
use crate::logos::{self, Logos, ROW_TILE, TileStyle};
use crate::settings::{Tab, key_source};
use crate::{Act, StudioApp, theme};

/// Longest destination name, in characters.
pub const MAX_NAME: usize = 128;
/// Longest server address, in bytes.
pub const MAX_SERVER: usize = 4096;
/// How often the window is drawn again while a connection check runs.
const CHECK_REPAINT: Duration = Duration::from_millis(250);
/// How often the window looks again at a Go-live check that waits for the free space.
const GO_LIVE_REPAINT: Duration = Duration::from_millis(15);

/// What the Streaming window keeps between frames: the destination whose details are open, with
/// the text typed into them.
#[derive(Default)]
pub(crate) struct StreamingPanel {
    pub(crate) editing: Option<Edit>,
    /// The name of the profile the open details belong to.
    pub(crate) profile: String,
    /// The decoded service logos.
    pub(crate) logos: Logos,
    /// Whether Go Live to N was pressed in the last frame.
    pub(crate) go_live: bool,
    /// When the Go-live check that is still waiting for the free space was started.
    pub(crate) checking: Option<Instant>,
    /// What the last Go Live to N found.
    pub(crate) golive: Option<GoLiveCheck>,
    /// The Settings tab a problem's Open Settings asked for in the last frame.
    pub(crate) open_settings: Option<Tab>,
}

/// Most items of the Go-live check the window lists.
const MAX_CHECK_LINES: usize = 64;

/// One item of the Go-live check as the window lists it: its English sentence with `{name}`
/// placeholders and their values, and for a problem the command that fixes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CheckLine {
    /// The item's id, such as `encoder` or `recording_space`.
    pub(crate) id: String,
    pub(crate) template: String,
    pub(crate) args: Vec<(String, String)>,
    pub(crate) fix: Option<String>,
    /// The source the item is about.
    pub(crate) source: Option<u64>,
}

impl CheckLine {
    fn of(item: &Value) -> CheckLine {
        let template = item.get("template").or_else(|| item.get("text")).and_then(Value::as_str).unwrap_or_default().to_string();
        let args = item
            .get("args")
            .and_then(Value::as_object)
            .map(|a| a.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect())
            .unwrap_or_default();
        let fix = item.get("fix").and_then(Value::as_str).map(str::to_string);
        let id = item.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        CheckLine { id, template, args, fix, source: item.get("source").and_then(Value::as_u64) }
    }

    /// The sentence in the interface language: its template's `@msg` row, with the values filled in.
    pub(crate) fn text(&self) -> String {
        let template = crate::i18n::I18N.tc(crate::i18n::lang(), "@msg", &self.template);
        let args: Vec<(&str, &str)> = self.args.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        craft_i18n::fmt(template, &args)
    }
}

/// What Go Live to N found: the check's problems and the items it could not confirm, and whether
/// the stream was started.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct GoLiveCheck {
    pub(crate) problems: Vec<CheckLine>,
    /// The items that read unknown, such as a destination that has not connected yet.
    pub(crate) unknown: Vec<CheckLine>,
    /// How many problems past [`MAX_CHECK_LINES`] the window does not list.
    pub(crate) more_problems: usize,
    /// How many unknown items past [`MAX_CHECK_LINES`] the window does not list.
    pub(crate) more_unknown: usize,
    /// How many sources that are not working the check left out of its list.
    pub(crate) more_sources: u64,
    /// Why the check could not be read or the stream did not start.
    pub(crate) error: Option<Msg>,
    pub(crate) started: bool,
}

impl GoLiveCheck {
    /// The result of `golive.check`. An item whose state is not `pass` or `unknown` is a problem,
    /// and a result whose problem count does not match its items is an error, so nothing the
    /// window cannot read is taken as a pass.
    pub(crate) fn of(v: &Value) -> GoLiveCheck {
        let mut check = GoLiveCheck { more_sources: v.get("more_sources").and_then(Value::as_u64).unwrap_or(0), ..GoLiveCheck::default() };
        let Some(items) = v.get("items").and_then(Value::as_array) else {
            check.error = Some(tmsg!("the Go-live check gave no items"));
            return check;
        };
        let mut problems = 0usize;
        for item in items {
            match item.get("state").and_then(Value::as_str) {
                Some("pass") => continue,
                Some("unknown") => {
                    if check.unknown.len() < MAX_CHECK_LINES {
                        check.unknown.push(CheckLine::of(item));
                    } else {
                        check.more_unknown = check.more_unknown.saturating_add(1);
                    }
                }
                _ => {
                    problems = problems.saturating_add(1);
                    if check.problems.len() < MAX_CHECK_LINES {
                        check.problems.push(CheckLine::of(item));
                    } else {
                        check.more_problems = check.more_problems.saturating_add(1);
                    }
                }
            }
        }
        let counted = v.get("problems").and_then(Value::as_u64).and_then(|n| usize::try_from(n).ok());
        if counted != Some(problems) {
            check.error = Some(tmsg!("the Go-live check's result could not be read"));
        }
        check
    }

    /// Whether nothing stops the stream: no problem and no error. Items that read unknown do not
    /// stop it.
    pub(crate) fn ready(&self) -> bool {
        self.error.is_none() && self.problems.is_empty()
    }
}

/// The label of the button for the command `fix` of a problem; `None` for `view.streaming`, whose
/// window is this one, and for a command the window has no button for.
pub(crate) fn fix_label(fix: &str) -> Option<&'static str> {
    match fix {
        "app.settings" => Some(tl!("Open Settings")),
        "captions.toggle" => Some(tl!("Turn On Live Captions")),
        "selection.properties" => Some(tl!("Open Properties")),
        _ => None,
    }
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
            KeyChoice::None => "No separate key",
            KeyChoice::Profile => "The profile's stream key",
            KeyChoice::Held => "A key for this session only",
        }
    }

    /// Where the choice's key is kept, in English, as its card says it.
    pub(crate) fn description(self) -> &'static str {
        match self {
            KeyChoice::None => "Any key is the last part of the server address, which is saved in the profile file as plain text.",
            KeyChoice::Profile => {
                "Typed in Settings › Stream and saved in the profile file as plain text. Anyone who can read that file can read the key."
            }
            KeyChoice::Held => "Held in memory until you forget it or quit. It is not written to the profile.",
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

/// What destination `d` is doing, in the interface language, and the tone of its line (`Neutral`
/// for the plain text color): from `live` while the stream runs, otherwise whether starting the
/// stream sends to it.
pub(crate) fn status_text(d: &Destination, running: bool, live: Option<&DestinationState>) -> (String, Tone) {
    match (running, live) {
        (true, Some(s)) => match &s.status {
            DestinationStatus::Connecting => (tl!("Connecting").to_string(), Tone::Neutral),
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
            DestinationStatus::Stopped => (tl!("Stopped").to_string(), Tone::Neutral),
            DestinationStatus::Failed(failure) => (craft_i18n::fmt(tl!("Failed: {error}"), &[("error", cause_text(failure.cause))]), Tone::Warn),
        },
        (true, None) => (tl!("Not in this stream; it is sent to from the next start").to_string(), Tone::Neutral),
        (false, _) if d.enabled => (tl!("Sent to when the stream starts").to_string(), Tone::Neutral),
        (false, _) => (tl!("Off").to_string(), Tone::Neutral),
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
pub(crate) fn sending(s: &DestinationState) -> bool {
    !matches!(s.status, DestinationStatus::Stopped | DestinationStatus::Failed(_))
}

/// The name a destination's row shows: its own name, else its service's name, else its id.
pub(crate) fn destination_title(d: &Destination) -> String {
    match services::service(&d.service) {
        _ if !d.name.trim().is_empty() => d.name.clone(),
        Some(svc) => svc.name.to_string(),
        None => d.id.clone(),
    }
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
    go_live_row(ui, view, panel);
    if let Some(tab) = panel.golive.as_ref().and_then(|check| go_live_result(ui, check, acts)) {
        panel.open_settings = Some(tab);
    }
    Divider::horizontal().show(ui);
    let count = view.stream.destinations.len();
    let _ = Row::new().show(ui, |ui, _| {
        let _ = Text::new(tl!("Add Destination")).role(TypeRole::BodyStrong).show(ui);
        let shown = craft_i18n::fmt(tl!("{n} of {max} destinations"), &[("n", &count.to_string()), ("max", &MAX_DESTINATIONS.to_string())]);
        let _ = Text::new(shown).shade(Shade::Muted).show(ui);
    });
    add_row(ui, count >= MAX_DESTINATIONS, &mut panel.logos, acts);
    if count == 0 {
        let _ = Text::new(tl!("No destinations yet. Add one to stream to it.")).shade(Shade::Muted).show(ui);
    }
    if view.running {
        let _ = Text::new(tl!("Server and key changes are used from the next start of the stream.")).shade(Shade::Muted).show(ui);
    }
    Divider::horizontal().show(ui);
    for d in &view.stream.destinations {
        let live = view.live.iter().find(|s| s.id == d.id);
        let _ = Stack::new().fill_width().id_salt(("destination", &d.id)).show(ui, |ui, _| row(ui, view, d, live, panel, acts));
        Divider::horizontal().show(ui);
    }
}

/// Go Live to N, where N is the number of destinations that are on. A click asks the window to run
/// the Go-live check and start the stream when nothing stops it; the button is off while the
/// stream runs, while no destination is on and while the check runs.
fn go_live_row(ui: &mut Ui, view: &View, panel: &mut StreamingPanel) {
    let on = view.stream.destinations.iter().filter(|d| d.enabled).count();
    let checking = panel.checking.is_some();
    let _ = Row::new().show(ui, |ui, _| {
        let label = craft_i18n::fmt(tl!("Go Live to {n}"), &[("n", &on.to_string())]);
        let mut tip =
            TooltipSpec::new(tl!("Runs the Go-live check, then starts the stream to every destination that is on when the check finds no problem"));
        if view.running {
            tip = tip.disabled_text(tl!("The stream is running"));
        } else if on == 0 {
            tip = tip.disabled_text(tl!("Turn on a destination first"));
        } else if checking {
            tip = tip.disabled_text(tl!("Running the Go-live check…"));
        }
        let enabled = !view.running && on > 0 && !checking;
        let button = Button::new(label).variant(Variant::PRIMARY).enabled(enabled).tooltip(tip).id_salt("go-live");
        if button.show(ui).clicked() {
            panel.go_live = true;
        }
        if checking {
            let _ = Spinner::new().show(ui);
            let _ = Text::new(tl!("Running the Go-live check…")).shade(Shade::Muted).show(ui);
        }
    });
}

/// What a click on the fix button of `line` does: Open Settings opens the Output tab for the
/// recording folder and the Stream tab otherwise, Open Properties opens the source's properties,
/// and Turn On Live Captions runs `captions.start`, which leaves running captions on.
pub(crate) enum FixAct {
    Settings(Tab),
    Act(Act),
}

pub(crate) fn fix_act(line: &CheckLine) -> Option<FixAct> {
    match (line.fix.as_deref()?, line.source) {
        ("app.settings", _) => Some(FixAct::Settings(if line.id == "recording_space" { Tab::Output } else { Tab::Stream })),
        ("selection.properties", Some(s)) => Some(FixAct::Act(Act::Properties(studiocraft_model::SourceId(s)))),
        ("captions.toggle", _) => Some(FixAct::Act(Act::run("captions.start", json!({})))),
        _ => None,
    }
}

/// What the last Go Live to N found: whether the stream started, each problem with the button for
/// its fix, and the items the check could not confirm. Returns the Settings tab a click asked for.
fn go_live_result(ui: &mut Ui, check: &GoLiveCheck, acts: &mut Vec<Act>) -> Option<Tab> {
    let mut settings = None;
    if let Some(e) = &check.error {
        let text = craft_i18n::fmt(tl!("The stream was not started: {error}"), &[("error", &e.shown())]);
        let _ = Text::new(text).tone(Tone::Warn).show(ui);
    } else if check.started {
        let _ = Text::new(tl!("The stream is starting")).tone(Tone::Good).show(ui);
    } else if !check.problems.is_empty() {
        let _ = Text::new(tl!("The stream was not started. Fix these first:")).role(TypeRole::BodyStrong).tone(Tone::Warn).show(ui);
    }
    for (i, line) in check.problems.iter().enumerate() {
        ui.push_id(("problem", i), |ui| {
            let _ = Row::new().show(ui, |ui, _| {
                let _ = Text::new(line.text()).tone(Tone::Warn).show(ui);
                let (Some(label), Some(act)) = (line.fix.as_deref().and_then(fix_label), fix_act(line)) else { return };
                if Button::new(label).size(ControlSize::Sm).id_salt("fix").show(ui).clicked() {
                    match act {
                        FixAct::Settings(tab) => settings = Some(tab),
                        FixAct::Act(a) => acts.push(a),
                    }
                }
            });
        });
    }
    if check.more_problems > 0 {
        let _ = Text::new(more_lines(check.more_problems)).tone(Tone::Warn).show(ui);
    }
    if check.more_sources > 0 {
        let text = craft_i18n::fmt(tl!("Sources not listed that are not working: {n}"), &[("n", &check.more_sources.to_string())]);
        let _ = Text::new(text).tone(Tone::Warn).show(ui);
    }
    if !check.unknown.is_empty() {
        let _ = Text::new(tl!("Not confirmed yet")).role(TypeRole::BodyStrong).show(ui);
        for line in &check.unknown {
            let _ = Text::new(line.text()).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
        }
        if check.more_unknown > 0 {
            let _ = Text::new(more_lines(check.more_unknown)).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
        }
    }
    settings
}

/// The line under a list that the window cut at [`MAX_CHECK_LINES`]: how many items it left out.
fn more_lines(n: usize) -> String {
    craft_i18n::fmt(tl!("And {n} more"), &[("n", &n.to_string())])
}

/// One tile of the row that adds a destination.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AddTile {
    /// The id of the streaming service the new destination is set up for; empty for a custom server.
    pub(crate) service: &'static str,
    /// The name under the tile.
    pub(crate) label: String,
    /// What a screen reader reads as the tile's name, and its tooltip.
    pub(crate) name: String,
}

impl AddTile {
    /// The `stream.destination_add` parameters the tile runs.
    pub(crate) fn params(&self) -> Value {
        if self.service.is_empty() { json!({}) } else { json!({"service": self.service}) }
    }
}

/// The tiles that add a destination: one per streaming service of [`SERVICES`], in its order,
/// then one for a server of the user's own choosing.
pub(crate) fn add_tiles() -> Vec<AddTile> {
    let mut tiles: Vec<AddTile> = SERVICES
        .iter()
        .map(|svc| AddTile {
            service: svc.id,
            label: svc.name.to_string(),
            name: craft_i18n::fmt(tl!("Add a destination to stream to {service}"), &[("service", svc.name)]),
        })
        .collect();
    tiles.push(AddTile { service: "", label: tl!("Custom server").to_string(), name: tl!("Add a destination for a custom server").to_string() });
    tiles
}

/// Add Destination: a row of tiles, one per streaming service with its logo where the service
/// allows it at this size and a plain tile otherwise, and one for a custom server. A click on a
/// tile adds a destination for it. Every tile is off while the profile holds `full` destinations.
fn add_row(ui: &mut Ui, full: bool, logos: &mut Logos, acts: &mut Vec<Act>) {
    let style = TileStyle::of(ui.ctx());
    let tiles = add_tiles();
    let font = egui::FontId::proportional(style.caption);
    // Every tile is as wide as the widest name, so the row reads as one set.
    let widest =
        tiles.iter().map(|t| ui.painter().layout_no_wrap(t.label.clone(), font.clone(), style.text).size()).fold(vec2(0.0, 0.0), |a, b| a.max(b));
    let size = vec2(widest.x.ceil().max(ROW_TILE) + 2.0 * style.pad, ROW_TILE + widest.y.ceil() + 2.5 * style.pad);
    let full_tip = craft_i18n::fmt(tl!("A profile holds at most {max} destinations"), &[("max", &MAX_DESTINATIONS.to_string())]);
    ui.horizontal_wrapped(|ui| {
        if full {
            ui.disable();
        }
        for tile in &tiles {
            if add_tile(ui, tile, size, &style, &font, logos).on_disabled_hover_text(&full_tip).clicked() {
                acts.push(Act::run("stream.destination_add", tile.params()));
            }
        }
    });
}

/// One tile of [`add_row`], `size` points large: its logo or plain tile over its name. A tile in a
/// disabled `ui` is drawn faded and does nothing.
fn add_tile(ui: &mut Ui, tile: &AddTile, size: egui::Vec2, style: &TileStyle, font: &egui::FontId, logos: &mut Logos) -> egui::Response {
    let enabled = ui.is_enabled();
    let (rect, r) = ui.allocate_exact_size(size, Sense::click());
    r.widget_info(|| WidgetInfo::labeled(Role::Button, enabled, &tile.name));
    if ui.is_rect_visible(rect) {
        let painter = ui.painter_at(rect.expand(style.focus.width.max(0.0)));
        let radius = egui::CornerRadius::same(style.row_radius);
        let fill = match (enabled, r.is_pointer_button_down_on(), r.hovered()) {
            (true, true, _) => Some(style.pressed),
            (true, false, true) => Some(style.hover),
            _ => None,
        };
        if let Some(fill) = fill {
            painter.rect_filled(rect, radius, fill);
        }
        let logo = egui::Rect::from_center_size(egui::pos2(rect.center().x, rect.top() + style.pad + ROW_TILE / 2.0), vec2(ROW_TILE, ROW_TILE));
        let tile_radius = style.row_radius;
        match logos::logo(tile.service).filter(|l| l.fits_row(ROW_TILE, ui.ctx().pixels_per_point())) {
            Some(l) => logos.paint(ui, logo, l, style, tile_radius),
            None => Logos::paint_plain(ui, logo, style, tile_radius),
        }
        let color = if enabled { style.text } else { style.text_disabled };
        let name = painter.layout_no_wrap(tile.label.clone(), font.clone(), color);
        let at = egui::pos2(rect.center().x - name.size().x / 2.0, logo.bottom() + style.pad / 2.0);
        painter.galley(at, name, color);
        if r.has_focus() {
            painter.rect_stroke(rect, radius, style.focus, egui::StrokeKind::Outside);
        }
    }
    if enabled { r.on_hover_text(&tile.name) } else { r }
}

/// `text` in `tone`, or in the plain text color for `Neutral`.
fn toned(text: Text, tone: Tone) -> Text {
    if tone == Tone::Neutral { text } else { text.tone(tone) }
}

fn row(ui: &mut Ui, view: &View, d: &Destination, live: Option<&DestinationState>, panel: &mut StreamingPanel, acts: &mut Vec<Act>) {
    let preset = services::service(&d.service);
    let title = destination_title(d);
    let _ = Row::new().fill_width().show(ui, |ui, _| {
        // A destination set up for a service shows the service's logo where it may stand beside
        // text at this size, and a plain tile otherwise.
        if preset.is_some() {
            let (tile, _) = ui.allocate_exact_size(vec2(ROW_TILE, ROW_TILE), Sense::hover());
            if ui.is_rect_visible(tile) {
                let style = TileStyle::of(ui.ctx());
                let radius = style.row_radius;
                match logos::logo(&d.service).filter(|l| l.fits_row(ROW_TILE, ui.ctx().pixels_per_point())) {
                    Some(l) => panel.logos.paint(ui, tile, l, &style, radius),
                    None => Logos::paint_plain(ui, tile, &style, radius),
                }
            }
        }
        // The check box takes the room the buttons leave, so they stand at the row's end edge.
        let mut on = d.enabled;
        let _ = Checkbox::new(Bind::bound(&mut on))
            .label(title.clone())
            .tooltip(tl!("Send to this destination when the stream starts"))
            .sizing(Sizing::FillWidth)
            .id_salt("enabled")
            .show(ui);
        if on != d.enabled {
            acts.push(Act::run("stream.destination_set", json!({"id": d.id, "enabled": on})));
        }
        if let Some(s) = live.filter(|_| view.running) {
            if sending(s) {
                if Button::new(tl!("Stop")).id_salt("stop").show(ui).clicked() {
                    acts.push(Act::run("stream.destination_stop", json!({"id": d.id})));
                }
            } else if Button::new(tl!("Start")).id_salt("start").show(ui).clicked() {
                acts.push(Act::run("stream.destination_start", json!({"id": d.id})));
            }
        }
        let editing = panel.editing.as_ref().is_some_and(|e| e.id == d.id);
        let mut open = editing;
        let _ = ToggleButton::new(tl!("Edit"), Bind::bound(&mut open)).id_salt("edit").show(ui);
        if open != editing {
            panel.editing = open.then(|| Edit::of(d));
        }
        // A destination the running stream sends to says why it cannot be removed.
        let can_remove = !(view.running && live.is_some_and(sending));
        let mut remove = Button::new(tl!("Remove")).enabled(can_remove).id_salt("remove");
        if !can_remove {
            let reason = tl!("Stop this destination before removing it");
            remove = remove.tooltip(TooltipSpec::new(reason).disabled_text(reason));
        }
        if remove.show(ui).clicked() {
            acts.push(Act::run("stream.destination_remove", json!({"id": d.id})));
        }
    });
    if let Some(svc) = preset {
        let _ = Text::new(craft_i18n::fmt(tl!("Stream to {service}"), &[("service", svc.name)])).role(TypeRole::Caption).show(ui);
    }
    let server = if d.server.trim().is_empty() { tl!("No server set").to_string() } else { shown_server(&d.server) };
    let _ = Text::new(server).role(TypeRole::Mono).elide(Elide::Middle).show(ui);
    let _ = Text::new(panel_key_source(&d.key)).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
    if let KeyRef::Held(name) = &d.key
        && !(view.held)(name)
    {
        let _ = Text::new(tl!("No key is held for this session yet")).role(TypeRole::Caption).tone(Tone::Warn).show(ui);
    }
    let (text, tone) = status_text(d, view.running, live);
    let mut status = toned(Text::new(text), tone);
    if let Some(detail) = status_detail(view.running, live) {
        status = status.tooltip(TooltipSpec::new(detail));
    }
    let _ = status.show(ui);
    check_row(ui, view, d, acts);
    let StreamingPanel { editing, logos, .. } = panel;
    if let Some(e) = editing.as_mut().filter(|e| e.id == d.id) {
        let close = details(ui, e, &title, view, logos, acts);
        if close {
            *editing = None;
        }
    }
}

/// Check Connection for destination `d`, and what its last check found, with the evidence in a
/// section that opens.
fn check_row(ui: &mut Ui, view: &View, d: &Destination, acts: &mut Vec<Act>) {
    let check = view.checks.iter().find(|(id, _)| *id == d.id).map(|(_, c)| c);
    let running = matches!(check, Some(CheckState::Running { .. }));
    let _ = Row::new().show(ui, |ui, _| {
        let has_server = !d.server.trim().is_empty();
        let mut tip = TooltipSpec::new(tl!(
            "Asks this computer whether it has a route to the server, looks the server's name up, and connects to the server on its \
             port and on port 443, with a secure connection where the address uses one. For a service that publishes them, it also \
             reads the service's list of ingest servers and status summary and connects to two of its other ingest servers. It sends \
             no stream key and nothing of the address after the server's name. Each server it contacts sees this computer's address, \
             and a secure connection sends the server's name unencrypted."
        ));
        if !has_server {
            tip = tip.disabled_text(tl!("Set a server for this destination first"));
        }
        let button = Button::new(tl!("Check Connection")).size(ControlSize::Sm).enabled(has_server && !running).tooltip(tip).id_salt("check");
        if button.show(ui).clicked() {
            acts.push(Act::run("stream.diagnose", json!({"id": d.id})));
        }
        if running {
            let _ = Spinner::new().show(ui);
            let _ = Text::new(tl!("Checking the connection…")).shade(Shade::Muted).show(ui);
        }
    });
    let Some(CheckState::Done(diag)) = check else { return };
    let tone = if diag.verdict == Verdict::Reachable { Tone::Good } else { Tone::Warn };
    let _ = Text::new(verdict_msg(diag).shown()).tone(tone).show(ui);
    if diag.automatic {
        let _ = Text::new(tl!("Checked by itself after the destination failed")).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
    }
    let _ = Section::new(tl!("What the check found")).initially_open(false).id_salt("check-evidence").show(ui, |ui, _| {
        for e in &diag.evidence {
            let _ = Text::new(evidence_text(e)).role(TypeRole::Caption).show(ui);
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
        let style = TileStyle::of(ui.ctx());
        logos.paint(ui, rect, l, &style, style.details_radius);
    }
    if r.on_hover_text(&hint).clicked() {
        acts.push(Act::OpenUrl(svc.source.to_string()));
    }
    ui.add_space(l.header_gap(tile));
}

/// The Service choices: Custom server, each service of [`SERVICES`], and `current` when this
/// version does not know it, so the choice shows the destination's own service.
fn service_choices(current: &str) -> Vec<Entry<String>> {
    let mut choices = vec![Entry::new(String::new(), tl!("Custom server"))];
    choices.extend(SERVICES.iter().map(|svc| Entry::new(svc.id.to_string(), svc.name)));
    if !current.is_empty() && services::service(current).is_none() {
        choices.push(Entry::new(current.to_string(), current.to_string()));
    }
    choices
}

/// The Stream key choice of the open details `e`: a card for each key source, in
/// [`KeyChoice::ALL`] order, that says where its key is kept. A click or an arrow key on a card
/// chooses its source.
fn key_cards(ui: &mut Ui, e: &mut Edit) {
    let _ = Text::new(tl!("Stream key")).role(TypeRole::BodyStrong).show(ui);
    let cards: Vec<OptionCard> = KeyChoice::ALL.iter().map(|c| OptionCard { title: tl!(c.label()), description: tl!(c.description()) }).collect();
    let style =
        theme::with_root(ui.ctx(), |cx| OptionCardStyle::new(cx, egui::FontId::new(cx.tokens.typography.body_strong.size, crate::fonts::semibold())));
    let selected = KeyChoice::ALL.iter().position(|c| *c == e.key).unwrap_or(KeyChoice::ALL.len());
    let id = ui.make_persistent_id("key-cards");
    if let Some(&choice) = OptionCards::new(id, &cards, selected, &style).show(ui).changed.and_then(|i| KeyChoice::ALL.get(i)) {
        e.key = choice;
    }
}

/// The open details of the destination named `title`; whether they closed.
fn details(ui: &mut Ui, e: &mut Edit, title: &str, view: &View, logos: &mut Logos, acts: &mut Vec<Act>) -> bool {
    let mut close = false;
    let _ = GroupBox::new(title.to_string(), None).id_salt("details").show(ui, |ui, _| {
        details_logo(ui, e, logos, acts);
        let _ = FieldGroup::new(title.to_string()).show(ui, |ui, _| {
            let _ = Field::new(tl!("Service")).show(ui, |ui, _| {
                let mut service = e.service.clone();
                let choices = service_choices(&e.service);
                let _ = Select::new(tl!("Service"), Bind::bound(&mut service), &choices).id_salt("service").show(ui);
                if service != e.service {
                    e.set_service(&service);
                }
            });
            let preset = services::service(&e.service);
            let _ = Field::new(tl!("Name")).show(ui, |ui, _| {
                let mut field = TextField::new(tl!("Name"), Bind::bound(&mut e.name)).limit(MAX_NAME).id_salt("name");
                if let Some(svc) = preset {
                    field = field.placeholder(svc.name);
                }
                field.show(ui)
            });
            // A service that shows each account its own server says where to find it.
            let own_server = preset.filter(|svc| svc.server.is_none());
            let server_hint = if own_server.is_some() { tl!("Copy it from your account") } else { tl!("rtmp://live.example.com/app") };
            let _ = Field::new(tl!("Server")).show(ui, |ui, _| {
                Stack::new().fill_width().show(ui, |ui, _| {
                    let _ = Row::new().fill_width().show(ui, |ui, _| {
                        // The address can hold a key, so the field hides its text until Show. The
                        // limit counts characters; `stream.destination_set` refuses an address of
                        // more than `MAX_SERVER` bytes.
                        let mut field = TextField::new(tl!("Server"), Bind::bound(&mut e.server))
                            .placeholder(server_hint)
                            .limit(MAX_SERVER)
                            .sizing(Sizing::FillWidth)
                            .id_salt("server");
                        if !e.show_server {
                            field = field.masked();
                        }
                        let _ = field.show(ui);
                        let _ = ToggleButton::new(tl!("Show"), Bind::bound(&mut e.show_server)).id_salt("show-server").show(ui);
                    });
                    if let Some(svc) = own_server {
                        let _ = Row::new().show(ui, |ui, _| {
                            let text = craft_i18n::fmt(tl!("{service} shows the server and key for your account."), &[("service", svc.name)]);
                            let _ = Text::new(text).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
                            if Link::new(tl!("Where to find them")).id_salt("find-server").show(ui).clicked() {
                                acts.push(Act::OpenUrl(svc.source.to_string()));
                            }
                        });
                    }
                })
            });
        });
        key_cards(ui, e);
        if e.key == KeyChoice::Held {
            let _ = FieldGroup::new(title.to_string()).id_salt("held-fields").show(ui, |ui, _| {
                let _ = Field::new(tl!("Session key name")).show(ui, |ui, _| {
                    TextField::new(tl!("Session key name"), Bind::bound(&mut e.held_name)).limit(MAX_DESTINATION_ID).id_salt("held-name").show(ui)
                });
                let _ = Field::new(tl!("Key for this session")).show(ui, |ui, _| {
                    Row::new().fill_width().show(ui, |ui, _| {
                        // A session key is held as typed, at any length.
                        let _ = TextField::new(tl!("Key for this session"), Bind::bound(&mut e.held_key))
                            .masked()
                            .limit(None)
                            .sizing(Sizing::FillWidth)
                            .id_salt("held-key")
                            .show(ui);
                        let name = e.held_name.trim().to_string();
                        let hold = Button::new(tl!("Hold")).enabled(!name.is_empty() && !e.held_key.is_empty()).id_salt("hold");
                        if hold.show(ui).clicked() {
                            acts.push(Act::HoldStreamKey { name: name.clone(), key: std::mem::take(&mut e.held_key) });
                        }
                        let forget = Button::new(tl!("Forget")).enabled(!name.is_empty() && (view.held)(&name)).id_salt("forget");
                        if forget.show(ui).clicked() {
                            acts.push(Act::HoldStreamKey { name, key: String::new() });
                        }
                    })
                });
            });
        }
        let _ = Row::new().show(ui, |ui, _| {
            // The details close when the change is made, so a refused one keeps what was typed.
            if Button::new(tl!("Save")).id_salt("save").show(ui).clicked() {
                acts.push(Act::run("stream.destination_set", e.params()));
            }
            if Button::new(tl!("Cancel")).id_salt("cancel").show(ui).clicked() {
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

    /// Close the window's open details and drop what Go Live to N found when another profile is in
    /// use, so the details are not saved into it.
    fn sync_streaming_profile(&mut self) {
        if self.streaming.as_ref().is_some_and(|p| p.profile != self.studio.profile().name) {
            self.streaming_profile_switched();
        }
    }

    /// Close the window's open details and drop what Go Live to N found, or is still checking,
    /// after another profile was put in use. A switch away and back to the same profile drops
    /// them too.
    pub(crate) fn streaming_profile_switched(&mut self) {
        let name = &self.studio.profile().name;
        if let Some(panel) = &mut self.streaming {
            panel.editing = None;
            panel.golive = None;
            panel.checking = None;
            panel.profile.clone_from(name);
        }
    }

    /// Go Live to N: starts the check of `golive.check` and looks at it once. See
    /// [`StudioApp::poll_go_live`].
    pub(crate) fn go_live(&mut self, ctx: &egui::Context, now: Instant) {
        let Some(panel) = &mut self.streaming else { return };
        panel.golive = None;
        panel.checking = Some(now);
        self.poll_go_live(ctx, now);
    }

    /// Look at the running Go-live check. The check does not wait for the free space in the
    /// recording folder: while that is still being read and less than `golive.check`'s wait has
    /// passed since the click, the window looks again in a later frame. Otherwise the check ends
    /// and, when it finds no problem, `stream.start` runs. Items that read unknown do not stop the
    /// stream, and the window lists them as not confirmed. What the check found goes in the
    /// window. A check whose window closed or whose profile changed is dropped.
    pub(crate) fn poll_go_live(&mut self, ctx: &egui::Context, now: Instant) {
        let Some(since) = self.streaming.as_ref().and_then(|p| p.checking) else { return };
        let (v, reading) = self.studio.golive_now();
        if reading && now.saturating_duration_since(since) < studiocraft_studio::GOLIVE_SPACE_WAIT {
            ctx.request_repaint_after(GO_LIVE_REPAINT);
            return;
        }
        if let Some(panel) = &mut self.streaming {
            panel.checking = None;
        }
        let mut check = GoLiveCheck::of(&v);
        // `stream.start` leaves a running stream as it is, so a stream started since the button
        // was drawn is not reported as started here.
        if check.ready() && !self.stream_running() {
            self.from_user = true;
            let started = self.engine_command(ctx, &craft_keymap::CommandRef::with_params("stream.start", json!({})));
            self.from_user = false;
            match started {
                Ok(_) => check.started = true,
                Err(e) => check.error = Some(e),
            }
        }
        if let Some(panel) = &mut self.streaming {
            panel.golive = Some(check);
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

    /// The Streaming window, a tool window over the main window while it is open. Closing it
    /// drops its open details.
    pub(crate) fn streaming_window(&mut self, ui: &mut Ui, acts: &mut Vec<Act>) {
        if self.streaming.is_none() {
            return;
        }
        self.sync_streaming_profile();
        // "The stream is starting" is shown while the stream starts, and goes once it runs, fails
        // or stops.
        let starting = self.studio.output(OutputKind::Streaming).is_some_and(|o| !o.stopping && o.status == OutputStatus::Starting);
        if let Some(panel) = &mut self.streaming
            && panel.golive.as_ref().is_some_and(|c| c.started)
            && !starting
        {
            panel.golive = None;
        }
        let running = self.stream_running();
        let live = self.live_destinations();
        let checks = self.destination_checks();
        // A check reports from its own thread, so the window looks again while one runs.
        if checks.iter().any(|(_, c)| matches!(c, CheckState::Running { .. })) {
            ui.ctx().request_repaint_after(CHECK_REPAINT);
        }
        let studio = &self.studio;
        let held = |name: &str| studio.stream_key_held(name);
        let view = View { stream: &studio.profile().stream, running, live: &live, held: &held, checks: &checks };
        let mut open = true;
        if let Some(panel) = &mut self.streaming {
            let title = tid!("view.streaming", "Streaming");
            let _ = Dialog::new(title).modeless().opened(Bind::bound(&mut open)).show(ui, |ui, _| show(ui, &view, panel, acts));
        }
        if !open {
            self.streaming = None;
        }
        // Open Settings shows the tab that fixes the problem, in a Settings window already open too.
        if let Some(tab) = self.streaming.as_mut().and_then(|p| p.open_settings.take()) {
            match &mut self.settings {
                Some(w) => w.tab = tab,
                None => self.open_settings(tab),
            }
        }
        let ctx = ui.ctx().clone();
        if self.streaming.as_mut().is_some_and(|p| std::mem::take(&mut p.go_live)) {
            self.go_live(&ctx, Instant::now());
        } else {
            self.poll_go_live(&ctx, Instant::now());
        }
    }
}

#[cfg(test)]
mod tests;
