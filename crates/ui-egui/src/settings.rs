//! The Settings window: General, Stream, Output, Audio, Video and Hotkeys, edited on a copy of
//! the profile, the Language setting, the update check at start and the graphics adapter (in the
//! Video tab's Hardware acceleration section), applied with OK or Apply.

// port: P16 (Settings on PreferencesWindow) replaces the egui combo boxes left here. The Presets
// and Match display boxes are menus under a fixed caption, which `Select` does not offer. The
// others show a saved value that is not among their options by its own name, and `Select` shows
// its placeholder for such a value: an encoder id the registry does not know, a language code in
// other letter case, an audio bitrate or frame rate outside the list, a monitoring device that is
// not connected or saved empty, and a hardware encoder saved empty.
#![expect(clippy::disallowed_types, reason = "craft-ui no longer has its combo box; Settings moves to `Select`")]

use std::collections::BTreeMap;

use craft_keymap::{Binding, CommandRef, Keymap, MAX_BINDING_TEXT, Overrides};
use craft_ui::contract::{Bind, Entry, Props as _, Show as _, ShowContainer as _};
use craft_ui::display::{Shade, Text};
use craft_ui::pickers::Select;
use craft_ui::sections::Section;
use craft_ui::style::TypeRole;
use egui::{ComboBox, RichText, Ui};
use studiocraft_audio_tracks::space::TRACK_BYTES_PER_SECOND;
use studiocraft_model::profile::DEFAULT_AUDIO_DEVICE;
use studiocraft_model::{
    AfterStop, AudioTracksSettings, AudioTracksWhen, FitMode, KeyRef, PauseAnchor, PauseAtZero, PauseSettings, Profile, ReconnectSettings,
    RecordingFormat, RecordingQuality, ReplaySettings, VideoSettings,
};
use studiocraft_platform::audio::{DeviceInfo, DeviceKind};
use studiocraft_plugin::{Availability, EncoderEnv, EncoderKind, Registry, Settings};

use studiocraft_render::device::{Candidate, Preference};

use crate::hardware;
use crate::i18n::{self, I18N, Msg};
use crate::kit::option_card::{OptionCard, OptionCardStyle, OptionCards};
use crate::theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tab {
    General,
    Stream,
    Output,
    Audio,
    Video,
    Hotkeys,
}

/// A path field that asked for a dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    RecordingFolder,
    Ffmpeg,
    AudioEditor,
    PauseMedia,
}

struct Hotkey {
    command: CommandRef,
    label: String,
    text: String,
    error: Option<String>,
}

pub struct SettingsWindow {
    pub draft: Profile,
    /// The Language setting: `auto` or a language code.
    pub language: String,
    /// The "Check for updates at start" setting.
    pub check_updates: bool,
    /// The "Use system title bar" setting.
    pub system_title_bar: bool,
    /// The tab that is shown.
    pub(crate) tab: Tab,
    show_key: bool,
    hotkeys: Vec<Hotkey>,
    /// Video encoders the configured ffmpeg offers (empty when it was not found).
    offered: Vec<String>,
    /// The configured ffmpeg's version text, or why it could not be read.
    pub(crate) ffmpeg_version: Result<String, String>,
    /// The output devices the system listed when the window opened.
    output_devices: Vec<DeviceInfo>,
    /// The displays the system listed at the last reading, for the canvas size.
    displays: Vec<studiocraft_platform::display::DisplayInfo>,
    /// The display last picked under Match display, which the canvas follows once applied with
    /// that display's size. `Some(None)` when the display the window is on was picked and no
    /// single listed display has its size; `None` until something is picked.
    pub canvas_display: Option<Option<studiocraft_platform::display::DisplayInfo>>,
    /// Reads the display list.
    list_displays: fn() -> Vec<studiocraft_platform::display::DisplayInfo>,
    /// When the Video page last read the display list, in the window's input time.
    displays_read: Option<f64>,
    /// The Graphics adapter setting.
    pub graphics_adapter: Preference,
    /// The adapters the window's graphics instance lists.
    pub adapters: Vec<Candidate>,
    /// The adapter the window draws with now, when it is known.
    pub drawing_with: Option<String>,
    /// What the user was told about the adapter the window opened on (a missing adapter, a
    /// fallback to software).
    pub gpu_notices: Vec<Msg>,
    /// The sources set to record their own track, for the audio tracks size estimate.
    pub track_sources: usize,
    /// The Pause screen section's first field takes the keyboard focus on the next frame.
    focus_pause: bool,
    /// Whether the Output tab's Advanced options are open. Each new window starts with them
    /// closed.
    advanced_open: bool,
    /// This window's number among the windows opened since the app started. The Advanced options
    /// fold takes it in its id, so a new window does not show the last window's fold closing.
    opened: u64,
}

/// The number of the next Settings window.
static NEXT_WINDOW: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The settings OK or Apply puts in force.
pub struct Apply {
    pub profile: Profile,
    /// Shortcut changes from the defaults.
    pub overrides: Overrides,
    /// The Language setting.
    pub language: String,
    /// The "Check for updates at start" setting.
    pub check_updates: bool,
    /// The "Use system title bar" setting.
    pub system_title_bar: bool,
    /// The Graphics adapter setting.
    pub graphics_adapter: Preference,
    /// The display picked under Match display, when one was picked; see
    /// [`SettingsWindow::canvas_display`].
    pub canvas_display: Option<Option<studiocraft_platform::display::DisplayInfo>>,
}

/// What the window asks for this frame.
#[derive(Default)]
pub struct Outcome {
    pub apply: Option<Apply>,
    pub close: bool,
    pub pick: Option<Pick>,
}

/// The tabs in the order the window shows them, with their English labels.
pub const TABS: [(Tab, &str); 6] = [
    (Tab::General, "General"),
    (Tab::Stream, "Stream"),
    (Tab::Output, "Output"),
    (Tab::Audio, "Audio"),
    (Tab::Video, "Video"),
    (Tab::Hotkeys, "Hotkeys"),
];

const FPS: [(&str, u32, u32); 8] =
    [("24", 24, 1), ("25", 25, 1), ("29.97", 30000, 1001), ("30", 30, 1), ("48", 48, 1), ("50", 50, 1), ("59.94", 60000, 1001), ("60", 60, 1)];
const SIZES: [(u32, u32); 7] = [(3840, 2160), (2560, 1440), (1920, 1080), (1600, 900), (1280, 720), (852, 480), (1080, 1920)];
const AUDIO_KBPS: [u32; 7] = [64, 96, 128, 160, 192, 256, 320];
/// Seconds between readings of the display list while the Video page is shown.
const DISPLAYS_EVERY: f64 = 1.0;

/// After an edit of the draft's canvas size: an output size that was a share of the old canvas
/// in [`studiocraft_model::OUTPUT_SCALES`] becomes the same share of the new one. A changed
/// output size, or one that was no such share, stays.
pub(crate) fn follow_canvas(before: VideoSettings, v: &mut VideoSettings) {
    let canvas_moved = (v.base_width, v.base_height) != (before.base_width, before.base_height);
    let output_kept = (v.output_width, v.output_height) == (before.output_width, before.output_height);
    if canvas_moved
        && output_kept
        && let Some(s) = before.output_scale()
    {
        (v.output_width, v.output_height) = v.scaled_output(s);
    }
}

/// An output scale's entry: its percentage and the output size it gives for `v`'s canvas.
fn scale_label(v: &VideoSettings, scale: (u32, u32)) -> String {
    let (w, h) = v.scaled_output(scale);
    let percent = (u64::from(scale.0) * 100 / u64::from(scale.1.max(1))).to_string();
    let (width, height) = (w.to_string(), h.to_string());
    craft_i18n::fmt(tl!("{percent}% ({width} × {height})"), &[("percent", &percent), ("width", &width), ("height", &height)])
}

fn bindings_text(keymap: &Keymap, cmd: &CommandRef) -> String {
    let home = craft_keymap::Scope::app();
    keymap.bindings(cmd).iter().map(|b| b.text(&home)).collect::<Vec<_>>().join(", ")
}

/// The shortcuts in a Hotkeys field, trimmed, without empty entries. Commas separate shortcuts,
/// and a comma can also be the key of one (`Ctrl+,`, `, ,`). Of the ways to split the text at
/// commas, the one with the fewest entries that do not read as a shortcut wins, then the one with
/// the fewest empty entries, then the one with the least text in entries that do not read as a
/// shortcut, then the one that keeps the fewest commas inside entries (`F2, A: Ctrl+K` is `F2` and
/// `A: Ctrl+K`, so a scope name with a comma cannot be typed here). An entry longer than
/// [`MAX_BINDING_TEXT`] bytes does not read as a shortcut, so the work grows with the length of
/// the text.
pub(crate) fn split_bindings(text: &str) -> Vec<&str> {
    // Cut points: the start, after each comma, and the end. Piece `a..b` runs from just after cut
    // `a` to just before cut `b`.
    let commas: Vec<usize> = text.match_indices(',').map(|(i, _)| i).collect();
    let from = |a: usize| if a == 0 { 0 } else { commas.get(a - 1).map_or(text.len(), |&i| i + 1) };
    let to = |b: usize| commas.get(b - 1).copied().unwrap_or(text.len());
    let cuts = commas.len() + 2;
    // For each cut, the best (entries that do not parse, empty entries, bytes in entries that do
    // not parse, commas inside entries) up to it, and the cut before.
    type Cost = (usize, usize, usize, usize);
    let mut best: Vec<(Cost, usize)> = vec![((0, 0, 0, 0), 0); cuts];
    for b in 1..cuts {
        let mut pick: Option<(Cost, usize)> = None;
        for a in (0..b).rev() {
            // Moving `a` back only adds text in front, so the trimmed piece never gets shorter.
            let trimmed = text.get(from(a)..to(b)).unwrap_or_default().trim();
            if a + 1 < b && trimmed.len() > MAX_BINDING_TEXT {
                break;
            }
            let (bad, empty, bad_bytes, kept) = best.get(a).map_or((0, 0, 0, 0), |c| c.0);
            let kept = kept + (b - a - 1);
            let cost = if trimmed.is_empty() {
                (bad, empty + 1, bad_bytes, kept)
            } else if Binding::parse(trimmed).is_ok() {
                (bad, empty, bad_bytes, kept)
            } else {
                (bad + 1, empty, bad_bytes.saturating_add(trimmed.len()), kept)
            };
            if pick.is_none_or(|(c, _)| cost < c) {
                pick = Some((cost, a));
            }
        }
        if let (Some(p), Some(slot)) = (pick, best.get_mut(b)) {
            *slot = p;
        }
    }
    let mut out = Vec::new();
    let mut b = cuts - 1;
    while b > 0 {
        let a = best.get(b).map_or(0, |c| c.1);
        let trimmed = text.get(from(a)..to(b)).unwrap_or_default().trim();
        if !trimmed.is_empty() {
            out.push(trimmed);
        }
        b = a;
    }
    out.reverse();
    out
}

fn probe(path: &str) -> (Vec<String>, Result<String, String>) {
    match studiocraft_platform::ffmpeg::locate(path) {
        Ok(p) => (
            studiocraft_platform::ffmpeg::encoders(&p),
            studiocraft_platform::ffmpeg::version(&p).ok_or_else(|| {
                craft_i18n::fmt(tl!("{program} did not report its version"), &[("program", &craft_i18n::ltr(&p.display().to_string()))])
            }),
        ),
        Err(e) => (Vec::new(), Err(e.to_string())),
    }
}

/// A shortcut list entry's label in the interface language: a command without parameters by its
/// command id, `Show Scene {n}` and `Present Staged Window {n}` by their templates, and any other
/// by its English label.
pub(crate) fn hotkey_label(command: &CommandRef, label: &str) -> String {
    if let Some(n) = command.params.get("index").and_then(serde_json::Value::as_u64) {
        match command.id.as_str() {
            "scene.select_index" => return craft_i18n::fmt(tl!("Show Scene {n}"), &[("n", &n.to_string())]),
            "present.window" => return craft_i18n::fmt(tl!("Present Staged Window {n}"), &[("n", &n.to_string())]),
            _ => {}
        }
    }
    if command.params.is_null() { i18n::command_label(&command.id, label).to_string() } else { tl!(label).to_string() }
}

/// The monitoring device `id` as the Audio tab shows it: the default device, a listed device by
/// its name, or a device the system does not list now.
fn device_name(id: &str, listed: &[DeviceInfo]) -> String {
    if id.is_empty() || id == DEFAULT_AUDIO_DEVICE {
        return tl!("Default").to_string();
    }
    match listed.iter().find(|d| d.id == id) {
        Some(d) => d.name.clone(),
        None => craft_i18n::fmt(tl!("{device} (not connected)"), &[("device", id)]),
    }
}

/// The name of a Language setting in the picker.
/// The English title and description of a recording quality's card in the Output tab.
pub(crate) fn quality_card(q: RecordingQuality) -> (&'static str, &'static str) {
    match q {
        RecordingQuality::Compact => ("Compact", "Smaller files for long sessions"),
        RecordingQuality::High => ("High", "A good fit for most recordings"),
        RecordingQuality::Pristine => ("Pristine", "Almost no loss, for editing later"),
        RecordingQuality::Lossless => ("Lossless", "No loss at all, in very large files"),
    }
}

/// The narrowest a Quality card is drawn beside the others. Four cards then stand in a row in
/// the Settings window at its default width.
const QUALITY_CARD_MIN_WIDTH: f32 = 120.0;

/// When audio tracks are written, as the Output tab shows it.
pub(crate) fn when_label(w: AudioTracksWhen) -> &'static str {
    match w {
        AudioTracksWhen::Recording => "A recording runs",
        AudioTracksWhen::Streaming => "The stream runs",
        AudioTracksWhen::Either => "A recording or the stream runs",
    }
}

/// What happens after a take of audio tracks, as the Output tab shows it.
pub(crate) fn after_stop_label(a: AfterStop) -> &'static str {
    match a {
        AfterStop::Nothing => "Nothing",
        AfterStop::ShowFolder => "Show the folder",
        AfterStop::OpenEditor => "Open in the audio editor",
    }
}

/// What the pause screen does when its countdown reaches zero, as the Stream tab shows it.
pub(crate) fn at_zero_label(z: PauseAtZero) -> &'static str {
    match z {
        PauseAtZero::Keep => "Keep the pause screen",
        PauseAtZero::Resume => "Resume the program",
    }
}

/// Where the pause screen's countdown text goes, as the Stream tab shows it.
pub(crate) fn anchor_label(a: PauseAnchor) -> &'static str {
    match a {
        PauseAnchor::TopLeft => "Top left",
        PauseAnchor::TopCenter => "Top center",
        PauseAnchor::TopRight => "Top right",
        PauseAnchor::MiddleLeft => "Middle left",
        PauseAnchor::Center => "Center",
        PauseAnchor::MiddleRight => "Middle right",
        PauseAnchor::BottomLeft => "Bottom left",
        PauseAnchor::BottomCenter => "Bottom center",
        PauseAnchor::BottomRight => "Bottom right",
    }
}

/// How the pause screen fits its picture or video, as the Stream tab shows it.
pub(crate) fn fit_label(f: FitMode) -> &'static str {
    match f {
        FitMode::Fit => "Fit inside the canvas",
        FitMode::Fill => "Fill the canvas",
        FitMode::Stretch => "Stretch to the canvas",
    }
}

/// A color button for a `#rrggbbaa` setting; an unreadable value shows as white.
fn color_button(ui: &mut Ui, value: &mut String) {
    let parsed = Settings::new().with("c", value.clone()).color("c").unwrap_or([255, 255, 255, 255]);
    let mut c = egui::Color32::from_rgba_unmultiplied(parsed[0], parsed[1], parsed[2], parsed[3]);
    if ui.color_edit_button_srgba(&mut c).changed() {
        *value = crate::forms::color_text(c.to_srgba_unmultiplied());
    }
}

/// The audio tracks' disk use in GiB an hour, and how many seconds the write buffer holds, both
/// as text, for `sources` recorded sources and the program when it is included. `None` when
/// there is no track to write.
fn tracks_estimate(t: &AudioTracksSettings, sources: usize) -> Option<(String, String)> {
    let tracks = sources + usize::from(t.include_program);
    if tracks == 0 {
        return None;
    }
    let per_second = tracks as f64 * TRACK_BYTES_PER_SECOND as f64;
    let gb = per_second * 3600.0 / f64::from(1u32 << 30);
    let chunks = studiocraft_audio_tracks::pool_chunks(t.buffer_mib as usize, tracks);
    let seconds = chunks as f64 * studiocraft_audio_tracks::collect::CHUNK_BYTES as f64 / per_second;
    Some((format!("{gb:.1}"), format!("{seconds:.0}")))
}

fn language_name(code: &str) -> String {
    if code == i18n::AUTO {
        let system = I18N.name(I18N.system_lang()).to_string();
        return craft_i18n::fmt(tl!("System ({language})"), &[("language", &system)]);
    }
    I18N.from_code(code).map_or_else(|| code.to_string(), |l| I18N.name(l).to_string())
}

impl SettingsWindow {
    pub fn new(profile: &Profile, keymap: &Keymap, tab: Tab, language: &str, check_updates: bool) -> SettingsWindow {
        let hotkeys = keymap
            .catalog()
            .iter()
            .map(|b| Hotkey {
                command: b.command.clone(),
                label: hotkey_label(&b.command, &b.label),
                text: bindings_text(keymap, &b.command),
                error: None,
            })
            .collect();
        let (offered, ffmpeg_version) = probe(&profile.output.ffmpeg_path);
        SettingsWindow {
            draft: profile.clone(),
            language: i18n::normalize(language).to_string(),
            check_updates,
            system_title_bar: false,
            tab,
            show_key: false,
            hotkeys,
            offered,
            ffmpeg_version,
            output_devices: studiocraft_platform::audio::devices(DeviceKind::Output),
            displays: studiocraft_platform::display::displays(),
            canvas_display: None,
            list_displays: studiocraft_platform::display::displays,
            displays_read: None,
            graphics_adapter: Preference::Auto,
            adapters: Vec::new(),
            drawing_with: None,
            gpu_notices: Vec::new(),
            track_sources: 0,
            focus_pause: false,
            advanced_open: false,
            opened: NEXT_WINDOW.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// The tab shown.
    #[cfg(test)]
    pub fn tab(&self) -> Tab {
        self.tab
    }

    /// Show the Stream tab and move the keyboard focus to the Pause screen section's first field
    /// on the next frame.
    pub fn show_pause_screen(&mut self) {
        self.tab = Tab::Stream;
        self.focus_pause = true;
    }

    /// Whether the Pause screen section still waits for the focus.
    #[cfg(test)]
    pub fn pause_focus_pending(&self) -> bool {
        self.focus_pause
    }

    /// Set a path chosen in a dialog.
    pub fn picked(&mut self, pick: Pick, path: &std::path::Path) {
        let text = path.to_string_lossy().into_owned();
        match pick {
            Pick::RecordingFolder => self.draft.output.folder = text,
            Pick::Ffmpeg => {
                self.draft.output.ffmpeg_path = text;
                (self.offered, self.ffmpeg_version) = probe(&self.draft.output.ffmpeg_path);
            }
            Pick::AudioEditor => self.draft.audio_tracks.editor_path = text,
            Pick::PauseMedia => self.draft.pause.media = text,
        }
    }

    /// The shortcut changes in the Hotkeys tab, checked; `None` when one does not read as a
    /// shortcut (the field shows why).
    fn overrides(&mut self, keymap: &Keymap) -> Option<Overrides> {
        let mut out = keymap.overrides().clone();
        let mut ok = true;
        for h in &mut self.hotkeys {
            h.error = None;
            let texts: Vec<String> = split_bindings(&h.text).into_iter().map(String::from).collect();
            for t in &texts {
                if let Err(e) = Binding::parse(t) {
                    h.error = Some(craft_i18n::fmt(tl!("`{shortcut}`: {error}"), &[("shortcut", t), ("error", &e.to_string())]));
                    ok = false;
                }
            }
            if h.text == bindings_text(keymap, &h.command) {
                continue;
            }
            let defaults: Vec<String> = keymap.default_bindings(&h.command).iter().map(|b| b.text(&craft_keymap::Scope::app())).collect();
            if texts == defaults {
                out.remove(&h.command.key());
            } else {
                out.insert(h.command.key(), texts);
            }
        }
        ok.then_some(out)
    }

    /// The video encoder list of [`listed_encoders`]; the ones the ffmpeg program does not list
    /// are shown disabled.
    fn encoder_combo(ui: &mut Ui, id: &str, registry: &Registry, offered: &[String], value: &mut String, streaming: bool) {
        let name = registry.encoder(value).map_or_else(|_| value.clone(), |e| tl!(e.name()).to_string());
        ComboBox::from_id_salt(id).selected_text(name).width(260.0).show_ui(ui, |ui| {
            for (e, available) in listed_encoders(registry, offered, streaming) {
                let name = tl!(e.name());
                let text = if available { name.to_string() } else { craft_i18n::fmt(tl!("{encoder} (not in this ffmpeg)"), &[("encoder", name)]) };
                ui.add_enabled_ui(available, |ui| ui.selectable_value(value, e.id().to_string(), text));
            }
        });
    }

    /// A grid row with the settings of the video encoder `encoder`, when it has any. Its values
    /// are saved under its id once the user changes one; until then its defaults apply.
    fn encoder_form(ui: &mut Ui, id: &str, registry: &Registry, encoder: &str, saved: &mut BTreeMap<String, Settings>) {
        let Ok(e) = registry.encoder(encoder) else { return };
        let schema = e.schema();
        if schema.fields.is_empty() {
            return;
        }
        let mut settings = saved.get(encoder).cloned().unwrap_or_default();
        ui.label("");
        if crate::forms::form(ui, egui::Id::unique(id), &schema, &mut settings).changed {
            saved.insert(encoder.to_string(), settings);
        }
        ui.end_row();
    }

    fn general(&mut self, ui: &mut Ui) {
        egui::Grid::new("general-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tl!("Language"));
            ComboBox::from_id_salt("language").selected_text(language_name(&self.language)).width(260.0).show_ui(ui, |ui| {
                for code in std::iter::once(i18n::AUTO).chain(i18n::languages()) {
                    ui.selectable_value(&mut self.language, code.to_string(), language_name(code));
                }
            });
            ui.end_row();
            ui.label(tl!("Updates"));
            ui.checkbox(&mut self.check_updates, tl!("Check for updates at start"));
            ui.end_row();
            ui.label(tl!("Window"));
            ui.vertical(|ui| {
                ui.checkbox(&mut self.system_title_bar, tl!("Use system title bar"));
                ui.label(RichText::new(tl!("Takes effect the next time StudioCraft starts.")).small().weak());
            });
            ui.end_row();
        });
    }

    fn stream(&mut self, ui: &mut Ui, registry: &Registry) {
        let s = &mut self.draft.stream;
        let kbps = |n: u32| craft_i18n::fmt(tl!("{n} kbps"), &[("n", &n.to_string())]);
        egui::Grid::new("stream-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            // The fields edit the first destination. The key field shows the profile's key when
            // that destination uses it and is empty otherwise, and the line under it says where
            // the key comes from; typing a key makes it use the profile's key.
            ui.label(tl!("Server"));
            let mut server = s.primary_server().to_string();
            if ui.add(egui::TextEdit::singleline(&mut server).hint_text("rtmp://live.example.com/app").desired_width(320.0)).changed() {
                s.primary_mut().server = server;
            }
            ui.end_row();
            ui.label(tl!("Stream key"));
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    let profile_key = s.destinations.first().is_none_or(|d| d.key == KeyRef::Profile);
                    let mut key = if profile_key { s.key.clone() } else { String::new() };
                    if ui.add(egui::TextEdit::singleline(&mut key).password(!self.show_key).desired_width(240.0)).changed() {
                        s.key = key;
                        s.primary_mut().key = KeyRef::Profile;
                    }
                    ui.toggle_value(&mut self.show_key, tl!("Show"));
                });
                let source = s.destinations.first().map_or(&KeyRef::Profile, |d| &d.key);
                ui.label(RichText::new(key_source(source)).small().weak());
            });
            ui.end_row();
            ui.label(tl!("Video bitrate"));
            ui.add(egui::DragValue::new(&mut s.video_bitrate_kbps).range(100..=200_000).speed(50.0).suffix(format!(" {}", tl!("kbps"))));
            ui.end_row();
            ui.label(tl!("Audio bitrate"));
            ComboBox::from_id_salt("audio-kbps").selected_text(kbps(s.audio_bitrate_kbps)).show_ui(ui, |ui| {
                for k in AUDIO_KBPS {
                    ui.selectable_value(&mut s.audio_bitrate_kbps, k, kbps(k));
                }
            });
            ui.end_row();
            ui.label(tl!("Video encoder"));
            Self::encoder_combo(ui, "stream-encoder", registry, &self.offered, &mut s.video_encoder, true);
            ui.end_row();
            Self::encoder_form(ui, "stream-encoder-settings", registry, &s.video_encoder, &mut s.encoder_settings);
            ui.label(tl!("Keyframe interval"));
            ui.add(egui::DragValue::new(&mut s.keyframe_interval_s).range(1..=20).suffix(format!(" {}", tl!("s"))));
            ui.end_row();
        });
        ui.add_space(12.0);
        Self::reconnect(ui, &mut s.reconnect);
        ui.add_space(12.0);
        Self::connection_check(ui, &mut s.check_service);
    }

    /// The Connection check section of the Stream tab: whether the check that runs after a
    /// destination fails also contacts the service's other hosts.
    fn connection_check(ui: &mut Ui, check_service: &mut bool) {
        ui.label(RichText::new(tl!("Connection check")).strong());
        ui.add_space(4.0);
        ui.checkbox(check_service, tl!("Also check the service's server list and status page after a failure"));
        ui.label(
            RichText::new(tl!(
                "When a destination fails three tries in a row or gives up, StudioCraft checks its server's name, route and port once. \
                 With this on, that check also reads the service's list of ingest servers and status summary and connects to two of its \
                 other ingest servers, so those hosts see this computer's address. Check Connection in the Streaming window always \
                 contacts them."
            ))
            .small()
            .weak(),
        );
    }

    /// The Automatic reconnect section of the Stream tab. The waits and the tries are disabled
    /// while automatic reconnect is off, and the longest wait is kept at least the first wait.
    fn reconnect(ui: &mut Ui, r: &mut ReconnectSettings) {
        ui.label(RichText::new(tl!("Automatic reconnect")).strong());
        ui.add_space(4.0);
        ui.checkbox(&mut r.enabled, tl!("Reconnect when a destination's connection fails or is lost"));
        let secs = format!(" {}", tl!("s"));
        ui.add_enabled_ui(r.enabled, |ui| {
            egui::Grid::new("reconnect-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label(tl!("First wait"));
                ui.add(egui::DragValue::new(&mut r.first_delay_s).range(ReconnectSettings::FIRST_DELAY_S).suffix(secs.clone()));
                ui.end_row();
                r.max_delay_s = r.max_delay_s.clamp(r.first_delay_s, ReconnectSettings::MAX_DELAY_S);
                ui.label(tl!("Longest wait"));
                ui.add(egui::DragValue::new(&mut r.max_delay_s).range(r.first_delay_s..=ReconnectSettings::MAX_DELAY_S).suffix(secs));
                ui.end_row();
                ui.label(tl!("Tries before giving up"));
                ui.add(egui::DragValue::new(&mut r.max_tries).range(ReconnectSettings::MAX_TRIES));
                ui.end_row();
            });
            ui.label(RichText::new(tl!("Each wait doubles up to the longest wait.")).small().weak());
        });
    }

    /// The Pause screen section of the Stream tab: what Pause Program shows in the program.
    fn pause_screen(&mut self, ui: &mut Ui, out: &mut Outcome) {
        ui.label(RichText::new(tl!("Pause screen")).strong());
        ui.add_space(4.0);
        let p = &mut self.draft.pause;
        let focus = &mut self.focus_pause;
        egui::Grid::new("pause-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tl!("Picture or video"));
            ui.horizontal(|ui| {
                let r = ui.add(egui::TextEdit::singleline(&mut p.media).hint_text(tl!("None")).desired_width(240.0));
                if std::mem::take(focus) {
                    r.request_focus();
                    r.scroll_to_me(Some(egui::Align::Center));
                }
                if ui.button(tl!("Browse…")).clicked() {
                    out.pick = Some(Pick::PauseMedia);
                }
            });
            ui.end_row();
            ui.label(tl!("Picture size"));
            let label = |f: FitMode| tl!(fit_label(f));
            ComboBox::from_id_salt("pause-fit").selected_text(label(p.fit)).width(220.0).show_ui(ui, |ui| {
                for f in [FitMode::Fit, FitMode::Fill, FitMode::Stretch] {
                    ui.selectable_value(&mut p.fit, f, label(f));
                }
            });
            ui.end_row();
            ui.label(tl!("Background"));
            color_button(ui, &mut p.background);
            ui.end_row();
            ui.label(tl!("Countdown"));
            ui.add(egui::DragValue::new(&mut p.countdown_s).range(PauseSettings::COUNTDOWN_S).speed(5.0).suffix(format!(" {}", tl!("s"))))
                .on_hover_text(tl!("0 shows no countdown"));
            ui.end_row();
            ui.label(tl!("Countdown text"));
            ui.vertical(|ui| {
                ui.add(egui::TextEdit::singleline(&mut p.text).char_limit(PauseSettings::MAX_TEXT).desired_width(240.0));
                if let Err(e) = PauseSettings::check_text(&p.text) {
                    ui.colored_label(ui.visuals().error_fg_color, Msg::plain(e.as_str()).shown());
                } else {
                    ui.label(RichText::new(tl!("{time} shows the time left")).small().weak());
                }
            });
            ui.end_row();
            ui.label(tl!("At zero"));
            let label = |z: PauseAtZero| tl!(at_zero_label(z));
            ComboBox::from_id_salt("pause-at-zero").selected_text(label(p.at_zero)).width(220.0).show_ui(ui, |ui| {
                for z in PauseAtZero::ALL {
                    ui.selectable_value(&mut p.at_zero, z, label(z));
                }
            });
            ui.end_row();
            ui.label(tl!("Text at zero"));
            ui.add(egui::TextEdit::singleline(&mut p.zero_text).char_limit(PauseSettings::MAX_TEXT).desired_width(240.0));
            ui.end_row();
            ui.label(tl!("Text position"));
            let label = |a: PauseAnchor| tl!(anchor_label(a));
            ComboBox::from_id_salt("pause-anchor").selected_text(label(p.anchor)).width(220.0).show_ui(ui, |ui| {
                for a in PauseAnchor::ALL {
                    ui.selectable_value(&mut p.anchor, a, label(a));
                }
            });
            ui.end_row();
            ui.label(tl!("Text size"));
            ui.add(egui::DragValue::new(&mut p.size_pct).range(PauseSettings::SIZE_PCT).suffix(format!(" {}", tl!("% of the canvas height"))));
            ui.end_row();
            ui.label(tl!("Font"));
            ui.add(egui::TextEdit::singleline(&mut p.font).hint_text(tl!("Font family")).desired_width(240.0));
            ui.end_row();
            ui.label(tl!("Text color"));
            color_button(ui, &mut p.color);
            ui.end_row();
            ui.label(tl!("Outline width"));
            ui.add(egui::DragValue::new(&mut p.outline).range(PauseSettings::OUTLINE));
            ui.end_row();
            ui.label(tl!("Outline color"));
            color_button(ui, &mut p.outline_color);
            ui.end_row();
            ui.label("");
            ui.checkbox(&mut p.mute_scene, tl!("Mute the program audio while paused"));
            ui.end_row();
        });
    }

    fn output(&mut self, ui: &mut Ui, registry: &Registry, out: &mut Outcome) {
        let o = &mut self.draft.output;
        egui::Grid::new("output-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tl!("Recording folder"));
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut o.folder).hint_text(tl!("Your Videos folder")).desired_width(240.0));
                if ui.button(tl!("Browse…")).clicked() {
                    out.pick = Some(Pick::RecordingFolder);
                }
            });
            ui.end_row();
            ui.label(tl!("Format"));
            let formats: Vec<_> = RecordingFormat::ALL.iter().map(|&f| Entry::new(f, f.extension().to_uppercase())).collect();
            let _ = Select::new(tl!("Format"), Bind::bound(&mut o.format), &formats).id_salt("format").show(ui);
            ui.end_row();
        });
        ui.add_space(8.0);
        Self::quality_cards(ui, &mut o.quality);
        ui.add_space(8.0);
        egui::Grid::new("output-encoder-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tl!("Video encoder"));
            Self::encoder_combo(ui, "record-encoder", registry, &self.offered, &mut o.video_encoder, false);
            ui.end_row();
            Self::encoder_form(ui, "record-encoder-settings", registry, &o.video_encoder, &mut o.encoder_settings);
            // Why the encoders that need ffmpeg are disabled shows while the ffmpeg program's
            // field is folded away.
            if let Err(e) = &self.ffmpeg_version {
                ui.label("");
                ui.colored_label(ui.visuals().error_fg_color, crate::i18n::Msg::plain(e.as_str()).shown());
                ui.end_row();
            }
            let r = &mut self.draft.replay;
            ui.label(tl!("Replay buffer length"));
            ui.add(egui::DragValue::new(&mut r.seconds).range(ReplaySettings::SECONDS).suffix(format!(" {}", tl!("s"))));
            ui.end_row();
            ui.label(tl!("Replay buffer memory limit"));
            ui.add(egui::DragValue::new(&mut r.memory_mb).range(ReplaySettings::MEMORY_MB).speed(16.0).suffix(format!(" {}", tl!("MB"))));
            ui.end_row();
        });
        ui.add_space(12.0);
        self.audio_tracks(ui);
        ui.add_space(12.0);
        self.advanced_output(ui, out);
    }

    /// The Quality cards of the Output tab: one card for each level of [`RecordingQuality::ALL`],
    /// with a short description. A click or an arrow key on a card chooses its level.
    fn quality_cards(ui: &mut Ui, quality: &mut RecordingQuality) {
        ui.label(tl!("Quality"));
        ui.add_space(4.0);
        let cards: Vec<OptionCard> = RecordingQuality::ALL
            .iter()
            .map(|&q| {
                let (title, description) = quality_card(q);
                OptionCard { title: tl!(title), description: tl!(description) }
            })
            .collect();
        let mut style = theme::with_root(ui.ctx(), |cx| {
            OptionCardStyle::new(cx, egui::FontId::new(cx.tokens.typography.body_strong.size, crate::fonts::semibold()))
        });
        style.min_width = QUALITY_CARD_MIN_WIDTH;
        let selected = RecordingQuality::ALL.iter().position(|q| q == quality).unwrap_or(RecordingQuality::ALL.len());
        let id = ui.make_persistent_id("quality-cards");
        if let Some(&q) = OptionCards::new(id, &cards, selected, &style).show(ui).changed.and_then(|i| RecordingQuality::ALL.get(i)) {
            *quality = q;
        }
    }

    /// The Audio tracks section of the Output tab.
    fn audio_tracks(&mut self, ui: &mut Ui) {
        ui.label(RichText::new(tl!("Audio tracks")).strong());
        ui.add_space(4.0);
        let t = &mut self.draft.audio_tracks;
        ui.checkbox(&mut t.enabled, tl!("Write each recorded source and the program mix to its own WAV file"));
        ui.add_enabled_ui(t.enabled, |ui| {
            egui::Grid::new("audio-tracks-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                ui.label(tl!("Write them while"));
                let whens: Vec<_> = AudioTracksWhen::ALL.iter().map(|&w| Entry::new(w, tl!(when_label(w)))).collect();
                let _ = Select::new(tl!("Write them while"), Bind::bound(&mut t.when), &whens).id_salt("audio-tracks-when").show(ui);
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut t.include_program, tl!("Include the program mix"));
                ui.end_row();
                ui.label(tl!("After a take"));
                let afters: Vec<_> = AfterStop::ALL.iter().map(|&a| Entry::new(a, tl!(after_stop_label(a)))).collect();
                let _ = Select::new(tl!("After a take"), Bind::bound(&mut t.after_stop), &afters).id_salt("audio-tracks-after-stop").show(ui);
                ui.end_row();
            });
            // With no track to write there is no disk use to estimate.
            if let Some((gb, seconds)) = tracks_estimate(t, self.track_sources) {
                let tracks = (self.track_sources + usize::from(t.include_program)).to_string();
                ui.weak(craft_i18n::fmt(
                    tl!("Audio tracks: {tracks}, about {gb} GiB an hour; the buffer holds {seconds} s of audio while the disk is slow"),
                    &[("tracks", &tracks), ("gb", &gb), ("seconds", &seconds)],
                ));
            }
        });
    }

    /// The Advanced options fold of the Output tab, closed when the window opens: the ffmpeg
    /// program with its version, and the audio tracks' write buffer, file size and audio editor.
    fn advanced_output(&mut self, ui: &mut Ui, out: &mut Outcome) {
        let _ = Section::new(tl!("Advanced options"))
            .open(Bind::bound(&mut self.advanced_open))
            .id_salt(("output-advanced", self.opened))
            .controls(|ui, _| {
                let _ = Text::new(tl!("ffmpeg program, write buffer, file size, audio editor")).role(TypeRole::Caption).shade(Shade::Muted).show(ui);
            })
            .show(ui, |ui, _| {
                let o = &mut self.draft.output;
                egui::Grid::new("output-advanced-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                    ui.label(tl!("ffmpeg program"));
                    ui.horizontal(|ui| {
                        let r = ui.add(egui::TextEdit::singleline(&mut o.ffmpeg_path).hint_text(tl!("ffmpeg on the PATH")).desired_width(240.0));
                        if ui.button(tl!("Browse…")).clicked() {
                            out.pick = Some(Pick::Ffmpeg);
                        }
                        if r.lost_focus() {
                            (self.offered, self.ffmpeg_version) = probe(&o.ffmpeg_path);
                        }
                    });
                    ui.end_row();
                    // An error shows above, under the video encoder.
                    if let Ok(v) = &self.ffmpeg_version {
                        ui.label("");
                        ui.weak(v);
                        ui.end_row();
                    }
                });
                ui.add_space(8.0);
                ui.label(RichText::new(tl!("Audio tracks")).strong());
                ui.add_space(4.0);
                let t = &mut self.draft.audio_tracks;
                ui.add_enabled_ui(t.enabled, |ui| {
                    egui::Grid::new("audio-tracks-advanced-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                        ui.label(tl!("Write buffer"));
                        ui.add(
                            egui::DragValue::new(&mut t.buffer_mib)
                                .range(AudioTracksSettings::BUFFER_MIB)
                                .speed(4.0)
                                .suffix(format!(" {}", tl!("MiB"))),
                        );
                        ui.end_row();
                        ui.label(tl!("File size"));
                        ui.add(egui::DragValue::new(&mut t.segment_gib).range(AudioTracksSettings::SEGMENT_GIB).suffix(format!(" {}", tl!("GiB"))));
                        ui.end_row();
                        ui.label(tl!("Audio editor"));
                        ui.horizontal(|ui| {
                            ui.add(
                                egui::TextEdit::singleline(&mut t.editor_path).hint_text(tl!("The audio editor on the PATH")).desired_width(240.0),
                            );
                            if ui.button(tl!("Browse…")).clicked() {
                                out.pick = Some(Pick::AudioEditor);
                            }
                        });
                        ui.end_row();
                    });
                });
            });
    }

    fn audio(&mut self, ui: &mut Ui) {
        let device = &mut self.draft.audio.monitoring_device;
        let listed = &self.output_devices;
        egui::Grid::new("audio-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tl!("Monitoring device"));
            ComboBox::from_id_salt("monitoring-device").selected_text(device_name(device, listed)).width(320.0).show_ui(ui, |ui| {
                ui.selectable_value(device, DEFAULT_AUDIO_DEVICE.to_string(), tl!("Default"));
                for d in listed {
                    ui.selectable_value(device, d.id.clone(), &d.name);
                }
            });
            ui.end_row();
        });
        ui.add_space(8.0);
        ui.weak(tl!("Sources set to monitoring in their Properties window play on this device."));
    }

    fn video(&mut self, ui: &mut Ui) {
        // On Linux the list arrives after the window opens, and displays can be connected while
        // the page is shown.
        let now = ui.ctx().input(|i| i.time);
        if self.displays_read.is_none_or(|read| now - read >= DISPLAYS_EVERY || now < read) {
            self.displays_read = Some(now);
            self.displays = (self.list_displays)();
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_secs_f64(DISPLAYS_EVERY));
        let v = &mut self.draft.video;
        let before = *v;
        let size_combo = |ui: &mut Ui, id: &str, w: &mut u32, h: &mut u32| {
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(w).range(16..=8192));
                ui.label("×");
                ui.add(egui::DragValue::new(h).range(16..=8192));
                ComboBox::from_id_salt(id).selected_text(tl!("Presets")).show_ui(ui, |ui| {
                    for (pw, ph) in SIZES {
                        if ui.selectable_label(*w == pw && *h == ph, format!("{pw} × {ph}")).clicked() {
                            (*w, *h) = (pw, ph);
                        }
                    }
                });
            });
        };
        egui::Grid::new("video-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tl!("Canvas size"));
            size_combo(ui, "base-size", &mut v.base_width, &mut v.base_height);
            ui.end_row();
            // Sizes from the displays: the one the window is on, then the listed ones. A display
            // too large or too small for a canvas is shown and can't be picked.
            let zoom = ui.ctx().zoom_factor();
            let here = ui.ctx().input(|i| crate::window_display(i.viewport(), zoom));
            // Each choice with the display the canvas then follows.
            let mut choices: Vec<(String, (u32, u32), Option<&studiocraft_platform::display::DisplayInfo>)> =
                Vec::with_capacity(self.displays.len() + 1);
            if let Some(s) = here {
                choices.push((tl!("The display this window is on").to_string(), s, crate::follow::of_size(&self.displays, s)));
            }
            choices.extend(self.displays.iter().map(|d| (d.name.clone(), (d.width, d.height), Some(d))));
            if !choices.is_empty() {
                ui.label(tl!("Match display"));
                let picked = &mut self.canvas_display;
                ComboBox::from_id_salt("match-display").selected_text(tl!("Choose a display")).show_ui(ui, |ui| {
                    for (name, (w, h), display) in &choices {
                        let fits = crate::canvas_for((*w, *h)).is_ok();
                        let on = v.base_width == *w && v.base_height == *h;
                        let (width, height) = (w.to_string(), h.to_string());
                        let label = craft_i18n::fmt(tl!("{name}: {width} × {height}"), &[("name", name), ("width", &width), ("height", &height)]);
                        if ui.add_enabled(fits, egui::Button::selectable(on, label)).clicked() {
                            (v.base_width, v.base_height) = (*w, *h);
                            *picked = Some(display.cloned());
                        }
                    }
                });
                ui.end_row();
            }
            follow_canvas(before, v);
            ui.label(tl!("Output size"));
            size_combo(ui, "output-size", &mut v.output_width, &mut v.output_height);
            ui.end_row();
            ui.label(tl!("Output scale"));
            let scale = v.output_scale();
            let current = scale.map_or_else(|| tl!("Custom").to_string(), |s| scale_label(v, s));
            ComboBox::from_id_salt("output-scale").selected_text(current).show_ui(ui, |ui| {
                for s in studiocraft_model::OUTPUT_SCALES {
                    if ui.selectable_label(scale == Some(s), scale_label(v, s)).clicked() {
                        (v.output_width, v.output_height) = v.scaled_output(s);
                    }
                }
            });
            ui.end_row();
            ui.label(tl!("Frames per second"));
            let current = FPS
                .iter()
                .find(|(_, n, d)| *n == v.fps_num && *d == v.fps_den)
                .map_or_else(|| format!("{:.2}", v.fps()), |(l, _, _)| (*l).to_string());
            ComboBox::from_id_salt("fps").selected_text(current).show_ui(ui, |ui| {
                for (label, n, d) in FPS {
                    if ui.selectable_label(v.fps_num == n && v.fps_den == d, label).clicked() {
                        (v.fps_num, v.fps_den) = (n, d);
                    }
                }
            });
            ui.end_row();
        });
        ui.add_space(8.0);
        ui.weak(tl!("Video settings can't change while recording, streaming or running the replay buffer."));
        ui.add_space(12.0);
        self.hardware(ui);
    }

    /// The Hardware acceleration section of the Video tab.
    fn hardware(&mut self, ui: &mut Ui) {
        ui.label(RichText::new(tl!("Hardware acceleration")).strong());
        ui.add_space(4.0);
        let listed = &self.adapters;
        let chosen = &mut self.graphics_adapter;
        let encoder = &mut self.draft.output.hardware_encoder;
        let offered = &self.offered;
        egui::Grid::new("hardware-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tl!("Graphics adapter"));
            ui.vertical(|ui| {
                let mut choices = hardware::choices(listed);
                if !choices.contains(chosen) {
                    choices.push(chosen.clone());
                }
                let adapters: Vec<_> = choices.into_iter().map(|c| Entry::new(c.clone(), hardware::label(&c, listed))).collect();
                let _ = Select::new(tl!("Graphics adapter"), Bind::bound(&mut *chosen), &adapters).id_salt("graphics-adapter").show(ui);
                ui.label(RichText::new(tl!("Takes effect the next time StudioCraft starts.")).small().weak());
            });
            ui.end_row();
            if let Some(name) = &self.drawing_with {
                ui.label("");
                ui.weak(craft_i18n::fmt(tl!("Drawing with {adapter}"), &[("adapter", name)]));
                ui.end_row();
            }
            ui.label(tl!("Hardware encoder"));
            ui.vertical(|ui| {
                ComboBox::from_id_salt("hardware-encoder").selected_text(hardware_encoder_label(encoder)).width(320.0).show_ui(ui, |ui| {
                    for id in hardware_encoder_choices(offered, encoder) {
                        let text = hardware_encoder_label(&id);
                        ui.selectable_value(encoder, id, text);
                    }
                });
                ui.label(
                    RichText::new(tl!(
                        "Used by recordings and streams whose encoder is Hardware. Automatic tests each hardware encoder with one frame and uses x264 when none works."
                    ))
                    .small()
                    .weak(),
                );
            });
            ui.end_row();
        });
        for n in &self.gpu_notices {
            ui.colored_label(ui.visuals().warn_fg_color, n.shown());
        }
    }

    fn hotkeys(&mut self, ui: &mut Ui) {
        ui.weak(tl!("Type shortcuts such as Ctrl+1 or Shift+F5; separate several with commas."));
        ui.add_space(4.0);
        egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
            egui::Grid::new("hotkeys-grid").num_columns(2).spacing([12.0, 6.0]).striped(true).show(ui, |ui| {
                for h in &mut self.hotkeys {
                    ui.label(&h.label);
                    ui.vertical(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut h.text).desired_width(200.0));
                        if let Some(e) = &h.error {
                            ui.colored_label(ui.visuals().error_fg_color, e);
                        }
                    });
                    ui.end_row();
                }
            });
        });
    }

    pub fn show(&mut self, ctx: &egui::Context, registry: &Registry, keymap: &Keymap) -> Outcome {
        let mut out = Outcome::default();
        let mut open = true;
        let window = egui::Window::new(tid!("app.settings", "Settings"))
            .id(egui::Id::unique("settings-window"))
            .open(&mut open)
            .collapsible(false)
            .default_size([560.0, 420.0]);
        window.show(ctx, |ui| {
            ui.horizontal(|ui| {
                for (t, label) in TABS {
                    ui.selectable_value(&mut self.tab, t, RichText::new(tl!(label)).strong());
                }
            });
            ui.separator();
            match self.tab {
                Tab::General => self.general(ui),
                Tab::Stream => {
                    // The tab scrolls when the window is shorter than its fields, and Pause Screen
                    // Settings… scrolls it to the Pause screen section.
                    let height = (ctx.content_rect().height() - 160.0).max(200.0);
                    egui::ScrollArea::vertical().id_salt("stream-tab").max_height(height).show(ui, |ui| {
                        self.stream(ui, registry);
                        ui.add_space(12.0);
                        self.pause_screen(ui, &mut out);
                    });
                }
                Tab::Output => {
                    // The tab scrolls when the window is shorter than its fields, as with the
                    // cards one above the other or the advanced options open.
                    let height = (ctx.content_rect().height() - 160.0).max(200.0);
                    egui::ScrollArea::vertical().id_salt("output-tab").max_height(height).show(ui, |ui| self.output(ui, registry, &mut out));
                }
                Tab::Audio => self.audio(ui),
                Tab::Video => self.video(ui),
                Tab::Hotkeys => self.hotkeys(ui),
            }
            ui.separator();
            ui.horizontal(|ui| {
                let ok = ui.button(tl!("OK")).clicked();
                let apply = ui.button(tl!("Apply")).clicked();
                if ui.button(tl!("Cancel")).clicked() {
                    out.close = true;
                }
                if ok || apply {
                    self.draft.video = self.draft.video.sanitized();
                    match self.overrides(keymap) {
                        Some(o) => {
                            out.apply = Some(Apply {
                                profile: self.draft.clone(),
                                overrides: o,
                                language: self.language.clone(),
                                check_updates: self.check_updates,
                                system_title_bar: self.system_title_bar,
                                graphics_adapter: self.graphics_adapter.clone(),
                                canvas_display: self.canvas_display.clone(),
                            });
                            out.close |= ok;
                        }
                        None => self.tab = Tab::Hotkeys,
                    }
                }
            });
        });
        out.close |= !open;
        out
    }
}

/// The Hardware encoder setting's choices: Automatic, Software only, then each hardware encoder of
/// this system that the ffmpeg program lists (every one when the list could not be read), and the
/// saved choice when it is none of those.
fn hardware_encoder_choices(offered: &[String], saved: &str) -> Vec<String> {
    let os = craft_hwenc::Os::current();
    let mut ids: Vec<String> = ["auto", "software"].map(String::from).to_vec();
    ids.extend(
        craft_hwenc::CANDIDATES
            .iter()
            .filter(|c| c.runs_on(os) && (offered.is_empty() || offered.iter().any(|o| o == c.ffmpeg_codec)))
            .map(|c| c.id.to_string()),
    );
    let saved = craft_hwenc::Preference::parse(saved);
    if !ids.iter().any(|i| i == saved.id()) {
        ids.push(saved.id().to_string());
    }
    ids
}

/// Where a destination's stream key comes from, as the Stream key field says it.
pub(crate) fn key_source(key: &KeyRef) -> String {
    match key {
        KeyRef::Profile => tl!("Saved in this profile as plain text").to_string(),
        KeyRef::Held(name) => {
            craft_i18n::fmt(tl!("Set for each session as {name} and never saved; a key typed here is saved in this profile"), &[("name", name)])
        }
        KeyRef::None => tl!("No separate key; a key at the end of the server address is used").to_string(),
    }
}

/// The shown name of a Hardware encoder setting value.
fn hardware_encoder_label(id: &str) -> String {
    match craft_hwenc::Preference::parse(id) {
        craft_hwenc::Preference::Auto => tl!("Automatic (the first that passes a test)").to_string(),
        craft_hwenc::Preference::Software => tl!("Software only").to_string(),
        craft_hwenc::Preference::Named(id) => craft_hwenc::candidate(&id).map_or(id, |c| tl!(c.name).to_string()),
    }
}

/// The video encoders a recording (`streaming` false) or a stream can choose, each with whether
/// it can start: encoders the output cannot run are left out (a stream runs the built-in
/// encoders only, and carries H.264 only), and ones the ffmpeg program does not list cannot start.
fn listed_encoders<'a>(registry: &'a Registry, offered: &[String], streaming: bool) -> Vec<(&'a dyn studiocraft_plugin::Encoder, bool)> {
    let env = EncoderEnv { builtin: true, ..EncoderEnv::ffmpeg(offered) };
    registry
        .encoders()
        .iter()
        .filter(|e| e.kind() == EncoderKind::Video && (!streaming || (e.streams() && e.ffmpeg().is_none())))
        .filter_map(|e| {
            let availability = e.availability(&env);
            (availability != Availability::NotRunHere).then_some((e.as_ref(), availability.can_start()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recordings_list_every_encoder_and_streams_the_built_in_ones() {
        let app = crate::StudioApp::boot(crate::store::Store::memory(), None, Vec::new());
        let registry = app.studio().registry();
        let offered = vec!["libx264".to_string()];
        let ids = |streaming| listed_encoders(registry, &offered, streaming).iter().map(|(e, ok)| (e.id(), *ok)).collect::<Vec<_>>();
        let recording = ids(false);
        assert!(recording.contains(&("h264", true)), "{recording:?}");
        assert!(recording.contains(&("x264", true)), "{recording:?}");
        let streaming = ids(true);
        assert!(streaming.contains(&("h264", true)), "{streaming:?}");
        assert!(!streaming.iter().any(|(id, _)| *id == "x264" || *id == "nvenc_h264"), "{streaming:?}");
        assert!(!streaming.iter().any(|(id, _)| *id == "nvenc_native_hevc"), "a stream carries H.264: {streaming:?}");
        // Without an ffmpeg program the built-in encoder can still record.
        assert!(listed_encoders(registry, &[], false).iter().any(|(e, ok)| e.id() == "h264" && *ok));
    }

    #[test]
    fn the_draft_output_follows_its_canvas_at_a_scale() {
        let before = VideoSettings { output_width: 1280, output_height: 720, ..VideoSettings::default() };
        let mut v = VideoSettings { base_width: 2560, base_height: 1440, ..before };
        follow_canvas(before, &mut v);
        assert_eq!((v.output_width, v.output_height), (1706, 960));
        // An output size the same edit changed, or one that is no share of the canvas, stays.
        let mut v = VideoSettings { base_width: 2560, base_height: 1440, output_width: 1000, ..before };
        follow_canvas(before, &mut v);
        assert_eq!((v.output_width, v.output_height), (1000, 720));
        let custom = VideoSettings { output_width: 1000, output_height: 500, ..VideoSettings::default() };
        let mut v = VideoSettings { base_width: 1080, base_height: 1920, ..custom };
        follow_canvas(custom, &mut v);
        assert_eq!((v.output_width, v.output_height), (1000, 500));
        // Without a canvas change nothing moves.
        let mut v = before;
        follow_canvas(before, &mut v);
        assert_eq!(v, before);
        assert_eq!(scale_label(&before, (2, 3)), "66% (1280 × 720)");
        assert_eq!(scale_label(&before, (1, 1)), "100% (1920 × 1080)");
    }

    /// The texts drawn by `f`.
    fn drawn(f: impl FnMut(&mut Ui)) -> Vec<String> {
        let ctx = egui::Context::default();
        let mut out = ctx.run_ui(egui::RawInput::default(), f);
        out.textures_delta.clear();
        out.shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_stream_key_field_says_where_its_key_comes_from() {
        let app = crate::StudioApp::boot(crate::store::Store::memory(), None, Vec::new());
        let registry = app.studio().registry();
        let km = crate::commands::keymap(Overrides::new());
        let mut p = Profile::default();
        p.stream.key = "profile-secret".into();
        let cases = [
            (None, key_source(&KeyRef::Profile)),
            (Some(KeyRef::Profile), key_source(&KeyRef::Profile)),
            (Some(KeyRef::Held("night".into())), key_source(&KeyRef::Held("night".into()))),
            (Some(KeyRef::None), key_source(&KeyRef::None)),
        ];
        for (key, want) in cases {
            if let Some(key) = &key {
                p.stream.primary_mut().key = key.clone();
            }
            let mut w = SettingsWindow::new(&p, &km, Tab::Stream, i18n::AUTO, true);
            w.show_key = true;
            let texts = drawn(|ui| w.stream(ui, registry));
            assert!(texts.contains(&want), "{key:?}: {want} in {texts:?}");
            // Only a destination that uses the profile's key shows it.
            let shows = key.is_none() || key == Some(KeyRef::Profile);
            assert_eq!(texts.iter().any(|t| t == "profile-secret"), shows, "{key:?}: {texts:?}");
            assert_eq!(w.draft, p, "{key:?}: drawing the tab changes nothing");
        }
        assert!(key_source(&KeyRef::Held("night".into())).contains("night"));
        let texts = [key_source(&KeyRef::Profile), key_source(&KeyRef::Held("x".into())), key_source(&KeyRef::None)];
        assert!(texts[0] != texts[1] && texts[1] != texts[2] && texts[0] != texts[2], "{texts:?}");
    }

    #[test]
    fn the_audio_tab_names_the_default_a_listed_device_and_a_missing_one() {
        let listed = vec![DeviceInfo { id: "Speakers (USB)".into(), name: "Speakers (USB)".into(), default: true }];
        assert_eq!(device_name(DEFAULT_AUDIO_DEVICE, &listed), tl!("Default"));
        assert_eq!(device_name("", &listed), tl!("Default"));
        assert_eq!(device_name("Speakers (USB)", &listed), "Speakers (USB)");
        assert_eq!(device_name("Headphones", &listed), craft_i18n::fmt(tl!("{device} (not connected)"), &[("device", "Headphones")]));
        assert!(device_name("Headphones", &listed).contains("Headphones"));
        assert!(TABS.iter().any(|(t, label)| *t == Tab::Audio && *label == "Audio"));
        let ctx = egui::Context::default();
        let mut w = SettingsWindow::new(&Profile::default(), &crate::commands::keymap(Overrides::new()), Tab::Audio, i18n::AUTO, true);
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| w.audio(ui));
        out.textures_delta.clear();
        assert_eq!(w.draft.audio.monitoring_device, DEFAULT_AUDIO_DEVICE, "drawing the tab changes nothing");
    }

    #[test]
    fn the_output_tab_estimates_the_audio_tracks_size_and_buffer() {
        let mut t = AudioTracksSettings::default();
        // 8 sources and the program: 9 × 384,000 bytes a second.
        assert_eq!(tracks_estimate(&t, 8), Some(("11.6".to_string(), "29".to_string())));
        t.include_program = false;
        t.buffer_mib = 16;
        assert_eq!(tracks_estimate(&t, 1), Some(("1.3".to_string(), "44".to_string())));
        // 20 tracks get 2 MiB each, more than the 16 MiB set.
        assert_eq!(tracks_estimate(&t, 20).map(|e| e.1), Some("5".to_string()));
        assert_eq!(tracks_estimate(&t, 0), None, "no tracks write nothing, so there is no estimate");
        let ctx = egui::Context::default();
        let mut p = Profile::default();
        p.audio_tracks.enabled = true;
        let mut w = SettingsWindow::new(&p, &crate::commands::keymap(Overrides::new()), Tab::Output, i18n::AUTO, true);
        w.track_sources = 2;
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| w.audio_tracks(ui));
        let texts: Vec<String> = out
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                _ => None,
            })
            .collect();
        out.textures_delta.clear();
        assert!(texts.iter().any(|t| t.starts_with("Audio tracks: 3, about 3.9 GiB an hour")), "{texts:?}");
        assert!(texts.iter().any(|t| t == "After a take") && texts.iter().any(|t| t == "Nothing"), "{texts:?}");
        assert_eq!(w.draft.audio_tracks, p.audio_tracks, "drawing the section changes nothing");
        // A program chosen in the dialog becomes the editor path.
        let editor = std::path::Path::new("Tools").join("editor.exe");
        w.picked(Pick::AudioEditor, &editor);
        assert_eq!(w.draft.audio_tracks.editor_path, editor.to_string_lossy());
    }

    /// The texts the Output tab of `w` draws in one 1280 × 800 frame with `events`, with the
    /// rectangle of each.
    fn output_frame(ctx: &egui::Context, w: &mut SettingsWindow, registry: &Registry, events: Vec<egui::Event>) -> Vec<(String, egui::Rect)> {
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0));
        let input = egui::RawInput { events, screen_rect: Some(screen), ..egui::RawInput::default() };
        let mut outcome = Outcome::default();
        let mut out = ctx.run_ui(input, |ui| w.output(ui, registry, &mut outcome));
        out.textures_delta.clear();
        assert!(outcome.pick.is_none() && outcome.apply.is_none(), "drawing the tab asks for nothing");
        out.shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some((t.galley.text().to_string(), t.galley.rect.translate(t.pos.to_vec2()))),
                _ => None,
            })
            .collect()
    }

    /// A primary click at `at` on the Output tab of `w`: the press in one frame and the release
    /// in the next.
    fn click_at(ctx: &egui::Context, w: &mut SettingsWindow, registry: &Registry, at: egui::Pos2) {
        let button =
            |pressed| egui::Event::PointerButton { pos: at, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        output_frame(ctx, w, registry, vec![egui::Event::PointerMoved(at), button(true)]);
        output_frame(ctx, w, registry, vec![button(false)]);
    }

    /// Where `text` is drawn in `texts`.
    fn place(texts: &[(String, egui::Rect)], text: &str) -> egui::Pos2 {
        let names: Vec<&str> = texts.iter().map(|(t, _)| t.as_str()).collect();
        texts.iter().find(|(t, _)| t == text).map(|(_, r)| r.center()).unwrap_or_else(|| panic!("no {text} in {names:?}"))
    }

    #[test]
    fn the_output_tab_chooses_the_quality_from_four_cards() {
        let app = crate::StudioApp::boot(crate::store::Store::memory(), None, Vec::new());
        let registry = app.studio().registry();
        let ctx = crate::test_context();
        let p = Profile::default();
        let mut w = SettingsWindow::new(&p, &crate::commands::keymap(Overrides::new()), Tab::Output, i18n::AUTO, true);
        let texts = output_frame(&ctx, &mut w, registry, vec![]);
        let shown: Vec<&str> = texts.iter().map(|(t, _)| t.as_str()).collect();
        for card in ["Compact", "High", "Pristine", "Lossless"] {
            assert!(shown.contains(&card), "a card for {card} in {shown:?}");
        }
        for description in [
            "Smaller files for long sessions",
            "A good fit for most recordings",
            "Almost no loss, for editing later",
            "No loss at all, in very large files",
        ] {
            assert!(shown.contains(&description), "{description} in {shown:?}");
        }
        assert_eq!(w.draft, p, "drawing the tab changes nothing");
        // A click on a card chooses its level, and the saved value is the same level as before.
        // The click goes where a frame after the first one drew the card.
        let pristine = place(&output_frame(&ctx, &mut w, registry, vec![]), "Pristine");
        click_at(&ctx, &mut w, registry, pristine);
        assert_eq!(w.draft.output.quality, RecordingQuality::Pristine);
        let compact = place(&output_frame(&ctx, &mut w, registry, vec![]), "Compact");
        click_at(&ctx, &mut w, registry, compact);
        assert_eq!(w.draft.output.quality, RecordingQuality::Compact);
    }

    #[test]
    fn rare_output_fields_fold_under_advanced_options() {
        let app = crate::StudioApp::boot(crate::store::Store::memory(), None, Vec::new());
        let registry = app.studio().registry();
        let ctx = crate::test_context();
        let mut p = Profile::default();
        p.audio_tracks.enabled = true;
        let mut w = SettingsWindow::new(&p, &crate::commands::keymap(Overrides::new()), Tab::Output, i18n::AUTO, true);
        w.ffmpeg_version = Ok("ffmpeg version 9.0".into());
        let rare = ["ffmpeg program", "ffmpeg version 9.0", "Write buffer", "File size", "Audio editor"];
        let common = ["Recording folder", "Format", "Quality", "Video encoder", "Replay buffer length", "Write them while", "After a take"];
        let texts = output_frame(&ctx, &mut w, registry, vec![]);
        let shown: Vec<&str> = texts.iter().map(|(t, _)| t.as_str()).collect();
        assert!(shown.contains(&"Advanced options"), "{shown:?}");
        for field in common {
            assert!(shown.contains(&field), "{field} shows with the fold closed: {shown:?}");
        }
        for field in rare {
            assert!(!shown.contains(&field), "{field} is folded: {shown:?}");
        }
        // A click on the fold's title opens it.
        let title = place(&texts, "Advanced options");
        click_at(&ctx, &mut w, registry, title);
        let mut shown = Vec::new();
        for _ in 0..30 {
            shown = output_frame(&ctx, &mut w, registry, vec![]).into_iter().map(|(t, _)| t).collect();
        }
        for field in rare.into_iter().chain(common) {
            assert!(shown.iter().any(|t| t == field), "{field} shows with the fold open: {shown:?}");
        }
        assert_eq!(w.draft, p, "opening the fold changes nothing");
        // Settings opened again in the same session starts with the fold closed.
        let mut again = SettingsWindow::new(&p, &crate::commands::keymap(Overrides::new()), Tab::Output, i18n::AUTO, true);
        again.ffmpeg_version = Ok("ffmpeg version 9.0".into());
        let shown: Vec<String> = output_frame(&ctx, &mut again, registry, vec![]).into_iter().map(|(t, _)| t).collect();
        assert!(!shown.iter().any(|t| t == "ffmpeg program"), "the fold starts closed again: {shown:?}");
    }

    #[test]
    fn an_ffmpeg_program_that_cannot_be_read_says_so_with_the_fold_closed() {
        let app = crate::StudioApp::boot(crate::store::Store::memory(), None, Vec::new());
        let registry = app.studio().registry();
        let ctx = crate::test_context();
        let mut w = SettingsWindow::new(&Profile::default(), &crate::commands::keymap(Overrides::new()), Tab::Output, i18n::AUTO, true);
        w.ffmpeg_version = Err("ffmpeg was not found on the PATH".into());
        let shown: Vec<String> = output_frame(&ctx, &mut w, registry, vec![]).into_iter().map(|(t, _)| t).collect();
        assert!(shown.iter().any(|t| t == "ffmpeg was not found on the PATH"), "{shown:?}");
        assert!(shown.iter().any(|t| t == "Advanced options") && !shown.iter().any(|t| t == "ffmpeg program"), "{shown:?}");
    }

    #[test]
    fn the_video_tab_lists_the_graphics_adapters_and_keeps_the_choice() {
        let ctx = egui::Context::default();
        let mut w = SettingsWindow::new(&Profile::default(), &crate::commands::keymap(Overrides::new()), Tab::Video, i18n::AUTO, true);
        w.graphics_adapter = Preference::LowPower;
        w.drawing_with = Some("GPU".into());
        w.gpu_notices = vec![hardware::notice_msg(&studiocraft_render::device::Notice::SoftwareOnly)];
        let mut out = ctx.run_ui(egui::RawInput::default(), |ui| w.video(ui));
        let texts: Vec<String> = out
            .shapes
            .iter()
            .filter_map(|s| match &s.shape {
                egui::Shape::Text(t) => Some(t.galley.text().to_string()),
                _ => None,
            })
            .collect();
        out.textures_delta.clear();
        for wanted in [tl!("Hardware acceleration"), tl!("Graphics adapter"), tl!("Power saving")] {
            assert!(texts.iter().any(|t| t == wanted), "{wanted} in {texts:?}");
        }
        assert!(texts.iter().any(|t| t.contains("GPU")), "{texts:?}");
        let notice = w.gpu_notices[0].shown();
        assert!(texts.contains(&notice), "the notice is shown: {texts:?}");
        assert_eq!(w.graphics_adapter, Preference::LowPower, "drawing the tab changes nothing");
        assert!(texts.iter().any(|t| t == tl!("Hardware encoder")), "{texts:?}");
        assert!(texts.iter().any(|t| t == tl!("Automatic (the first that passes a test)")), "the default is shown: {texts:?}");
    }

    #[test]
    fn the_video_page_reads_the_display_list_again_while_shown() {
        use studiocraft_platform::display::DisplayInfo;
        fn display(name: &str) -> DisplayInfo {
            DisplayInfo { id: name.into(), name: name.into(), width: 1920, height: 1080, primary: false }
        }
        fn none() -> Vec<DisplayInfo> {
            Vec::new()
        }
        fn one() -> Vec<DisplayInfo> {
            vec![display("DP-1")]
        }
        fn two() -> Vec<DisplayInfo> {
            vec![display("DP-1"), display("HDMI-A-1")]
        }
        let ctx = egui::Context::default();
        let mut w = SettingsWindow::new(&Profile::default(), &crate::commands::keymap(Overrides::new()), Tab::Video, i18n::AUTO, true);
        let draw = |w: &mut SettingsWindow, time: f64| {
            let input = egui::RawInput { time: Some(time), ..Default::default() };
            let mut out = ctx.run_ui(input, |ui| w.video(ui));
            out.textures_delta.clear();
        };
        // The window opened before the system answered.
        w.displays = Vec::new();
        w.list_displays = none;
        draw(&mut w, 10.0);
        assert!(w.displays.is_empty());
        // The answer arrives; the page shows it within a second, without reopening.
        w.list_displays = one;
        draw(&mut w, 10.5);
        assert!(w.displays.is_empty(), "read at most once a second");
        draw(&mut w, 11.0);
        assert_eq!(w.displays, one());
        // A display connected while the page is shown.
        w.list_displays = two;
        draw(&mut w, 12.0);
        assert_eq!(w.displays, two());
    }

    #[test]
    fn the_hardware_encoder_choices_are_this_systems_encoders_that_ffmpeg_lists() {
        let os = craft_hwenc::Os::current();
        let listed: Vec<String> = craft_hwenc::CANDIDATES.iter().filter(|c| c.runs_on(os)).take(1).map(|c| c.ffmpeg_codec.to_string()).collect();
        let choices = hardware_encoder_choices(&listed, "auto");
        assert_eq!(choices[..2], ["auto", "software"]);
        assert_eq!(choices.len(), 2 + listed.len(), "{choices:?}");
        let every = hardware_encoder_choices(&[], "");
        assert_eq!(every.len(), 2 + craft_hwenc::CANDIDATES.iter().filter(|c| c.runs_on(os)).count(), "an unread list shows every one: {every:?}");
        let saved = hardware_encoder_choices(&listed, "qsv_av1_gone");
        assert_eq!(saved.last().map(String::as_str), Some("qsv_av1_gone"), "the saved choice stays selectable");
        assert_eq!(hardware_encoder_label("qsv_av1_gone"), "qsv_av1_gone");
        assert_eq!(hardware_encoder_label("nvenc_h264"), tl!("NVIDIA NVENC H.264"));
    }

    #[test]
    fn changed_hotkeys_become_overrides_and_defaults_drop_out() {
        let km = crate::commands::keymap(Overrides::new());
        let mut w = SettingsWindow::new(&Profile::default(), &km, Tab::Hotkeys, i18n::AUTO, true);
        let rec = w.hotkeys.iter_mut().find(|h| h.command.id == "record.toggle").unwrap();
        rec.text = "Ctrl+Shift+R".into();
        let o = w.overrides(&km).unwrap();
        assert_eq!(o.get(&CommandRef::new("record.toggle").key()), Some(&["Ctrl+Shift+R".to_string()][..]));
        let km2 = crate::commands::keymap(o);
        assert_eq!(km2.bindings(&CommandRef::new("record.toggle")).len(), 1);

        let undo = w.hotkeys.iter_mut().find(|h| h.command.id == "edit.undo").unwrap();
        undo.text = "not a key+".into();
        assert!(w.overrides(&km).is_none());
        assert!(w.hotkeys.iter().any(|h| h.error.is_some()));
    }

    #[test]
    fn a_shortcut_with_the_comma_key_saves_and_opens_again() {
        let km = crate::commands::keymap(Overrides::new());
        let mut w = SettingsWindow::new(&Profile::default(), &km, Tab::Hotkeys, i18n::AUTO, true);
        let settings = w.hotkeys.iter_mut().find(|h| h.command.id == "app.settings").unwrap();
        settings.text = "Cmd+,, F2".into();
        let o = w.overrides(&km).unwrap();
        assert_eq!(o.get(&CommandRef::new("app.settings").key()), Some(&["Cmd+,".to_string(), "F2".to_string()][..]));
        assert!(w.hotkeys.iter().all(|h| h.error.is_none()));

        let km2 = crate::commands::keymap(o);
        assert_eq!(km2.bindings(&CommandRef::new("app.settings")).len(), 2);
        let mut again = SettingsWindow::new(&Profile::default(), &km2, Tab::Hotkeys, i18n::AUTO, true);
        let saved = again.overrides(&km2).expect("the saved text reads back and does not block a save");
        assert_eq!(saved.get(&CommandRef::new("app.settings").key()), Some(&["Cmd+,".to_string(), "F2".to_string()][..]));
        assert!(again.hotkeys.iter().all(|h| h.error.is_none()));
    }

    #[test]
    fn hotkey_fields_split_at_commas_that_are_not_keys() {
        assert_eq!(split_bindings("Ctrl+1, Shift+F5"), ["Ctrl+1", "Shift+F5"]);
        assert_eq!(split_bindings("Ctrl+1 , Shift+F5,"), ["Ctrl+1", "Shift+F5"]);
        assert_eq!(split_bindings("Ctrl+,"), ["Ctrl+,"]);
        assert_eq!(split_bindings("Ctrl+,, Ctrl+K"), ["Ctrl+,", "Ctrl+K"]);
        assert_eq!(split_bindings("Ctrl+K, Ctrl+Shift+,"), ["Ctrl+K", "Ctrl+Shift+,"]);
        assert_eq!(split_bindings("⌘,, ⇧⌘,"), ["⌘,", "⇧⌘,"]);
        assert_eq!(split_bindings(","), [","]);
        assert_eq!(split_bindings(", ,, A"), [", ,", "A"]);
        assert_eq!(split_bindings("Ctrl+, Ctrl+,"), ["Ctrl+, Ctrl+,"]);
        assert_eq!(split_bindings("Cmd++, F2"), ["Cmd++", "F2"]);
        assert_eq!(split_bindings("Ctrl+, , F2"), ["Ctrl+,", "F2"]);
        assert_eq!(split_bindings("Ctrl+1,, F2"), ["Ctrl+1", "F2"]);
        assert_eq!(split_bindings("Ctrl+1, not a key+"), ["Ctrl+1", "not a key+"]);
        assert_eq!(split_bindings("F2, A: Ctrl+K"), ["F2", "A: Ctrl+K"]);
        assert_eq!(split_bindings("A, B"), ["A", "B"]);
        let spaced = format!("Ctrl+,{}, F2", " ".repeat(200));
        assert_eq!(split_bindings(&spaced), ["Ctrl+,", "F2"]);
        assert!(split_bindings("").is_empty());
        assert!(split_bindings("  ").is_empty());
        for t in ["Ctrl+,", ",", ", ,", "Ctrl+, Ctrl+,", "⇧⌘,", "Cmd++", "F2, A: Ctrl+K"] {
            assert!(Binding::parse(t).is_ok(), "{t}");
        }
        assert_eq!(split_bindings(&"+,".repeat(2_000)).len(), 2_000);
        assert!(split_bindings(&",".repeat(4_000)).iter().all(|t| *t == ","));
        let long = "x".repeat(1_000_000);
        assert_eq!(split_bindings(&long), [long.as_str()]);
    }
}
