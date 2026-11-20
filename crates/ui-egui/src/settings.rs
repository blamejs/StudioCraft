//! The Settings window: General, Stream, Output, Audio, Video and Hotkeys, edited on a copy of
//! the profile, the Language setting, the update check at start and the graphics adapter (in the
//! Video tab's Hardware acceleration section), applied with OK or Apply.

use std::collections::BTreeMap;

use craft_keymap::{Binding, CommandRef, Keymap, Overrides};
use egui::{RichText, Ui};
use studiocraft_audio_tracks::space::TRACK_BYTES_PER_SECOND;
use studiocraft_model::profile::DEFAULT_AUDIO_DEVICE;
use studiocraft_model::{AudioTracksSettings, AudioTracksWhen, Profile, RecordingFormat, RecordingQuality, ReplaySettings};
use studiocraft_platform::audio::{DeviceInfo, DeviceKind};
use studiocraft_plugin::{Availability, EncoderEnv, EncoderKind, Registry, Settings};

use studiocraft_render::device::{Candidate, Preference};

use crate::hardware;
use crate::i18n::{self, I18N, Msg};

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
    tab: Tab,
    show_key: bool,
    hotkeys: Vec<Hotkey>,
    /// Video encoders the configured ffmpeg offers (empty when it was not found).
    offered: Vec<String>,
    /// The configured ffmpeg's version text, or why it could not be read.
    pub(crate) ffmpeg_version: Result<String, String>,
    /// The output devices the system listed when the window opened.
    output_devices: Vec<DeviceInfo>,
    /// The displays the system listed when the window opened, for the canvas size.
    displays: Vec<studiocraft_platform::display::DisplayInfo>,
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
}

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
const SIZES: [(u32, u32); 6] = [(3840, 2160), (2560, 1440), (1920, 1080), (1600, 900), (1280, 720), (852, 480)];
const AUDIO_KBPS: [u32; 7] = [64, 96, 128, 160, 192, 256, 320];

fn bindings_text(keymap: &Keymap, cmd: &CommandRef) -> String {
    let home = craft_keymap::Scope::app();
    keymap.bindings(cmd).iter().map(|b| b.text(&home)).collect::<Vec<_>>().join(", ")
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
/// command id, `Show Scene {n}` by its template, and any other by its English label.
fn hotkey_label(command: &CommandRef, label: &str) -> String {
    if command.id == "scene.select_index"
        && let Some(n) = command.params.get("index").and_then(serde_json::Value::as_u64)
    {
        return craft_i18n::fmt(tl!("Show Scene {n}"), &[("n", &n.to_string())]);
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
/// The English name of a recording quality in the Output tab.
pub(crate) fn quality_label(q: RecordingQuality) -> &'static str {
    match q {
        RecordingQuality::Compact => "Compact (small files)",
        RecordingQuality::High => "High",
        RecordingQuality::Pristine => "Pristine (large files)",
        RecordingQuality::Lossless => "Lossless (very large files)",
    }
}

/// When audio tracks are written, as the Output tab shows it.
pub(crate) fn when_label(w: AudioTracksWhen) -> &'static str {
    match w {
        AudioTracksWhen::Recording => "A recording runs",
        AudioTracksWhen::Streaming => "The stream runs",
        AudioTracksWhen::Either => "A recording or the stream runs",
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
            graphics_adapter: Preference::Auto,
            adapters: Vec::new(),
            drawing_with: None,
            gpu_notices: Vec::new(),
            track_sources: 0,
        }
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
        }
    }

    /// The shortcut changes in the Hotkeys tab, checked; `None` when one does not read as a
    /// shortcut (the field shows why).
    fn overrides(&mut self, keymap: &Keymap) -> Option<Overrides> {
        let mut out = keymap.overrides().clone();
        let mut ok = true;
        for h in &mut self.hotkeys {
            h.error = None;
            let texts: Vec<String> = h.text.split(',').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect();
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
        craft_ui::ComboBox::from_id_salt(id).selected_text(name).width(260.0).show_ui(ui, |ui| {
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
            craft_ui::ComboBox::from_id_salt("language").selected_text(language_name(&self.language)).width(260.0).show_ui(ui, |ui| {
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
            ui.label(tl!("Server"));
            ui.add(egui::TextEdit::singleline(&mut s.server).hint_text("rtmp://live.example.com/app").desired_width(320.0));
            ui.end_row();
            ui.label(tl!("Stream key"));
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut s.key).password(!self.show_key).desired_width(240.0));
                ui.toggle_value(&mut self.show_key, tl!("Show"));
            });
            ui.end_row();
            ui.label(tl!("Video bitrate"));
            ui.add(egui::DragValue::new(&mut s.video_bitrate_kbps).range(100..=200_000).speed(50.0).suffix(format!(" {}", tl!("kbps"))));
            ui.end_row();
            ui.label(tl!("Audio bitrate"));
            craft_ui::ComboBox::from_id_salt("audio-kbps").selected_text(kbps(s.audio_bitrate_kbps)).show_ui(ui, |ui| {
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
            craft_ui::ComboBox::from_id_salt("format").selected_text(o.format.extension().to_uppercase()).show_ui(ui, |ui| {
                for f in RecordingFormat::ALL {
                    ui.selectable_value(&mut o.format, f, f.extension().to_uppercase());
                }
            });
            ui.end_row();
            ui.label(tl!("Quality"));
            let label = |q: RecordingQuality| tl!(quality_label(q));
            craft_ui::ComboBox::from_id_salt("quality").selected_text(label(o.quality)).width(260.0).show_ui(ui, |ui| {
                for q in RecordingQuality::ALL {
                    ui.selectable_value(&mut o.quality, q, label(q));
                }
            });
            ui.end_row();
            ui.label(tl!("Video encoder"));
            Self::encoder_combo(ui, "record-encoder", registry, &self.offered, &mut o.video_encoder, false);
            ui.end_row();
            Self::encoder_form(ui, "record-encoder-settings", registry, &o.video_encoder, &mut o.encoder_settings);
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
            ui.label("");
            match &self.ffmpeg_version {
                Ok(v) => ui.weak(v),
                Err(e) => ui.colored_label(ui.visuals().error_fg_color, crate::i18n::Msg::plain(e.as_str()).shown()),
            };
            ui.end_row();
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
                let label = |w: AudioTracksWhen| tl!(when_label(w));
                craft_ui::ComboBox::from_id_salt("audio-tracks-when").selected_text(label(t.when)).width(260.0).show_ui(ui, |ui| {
                    for w in AudioTracksWhen::ALL {
                        ui.selectable_value(&mut t.when, w, label(w));
                    }
                });
                ui.end_row();
                ui.label("");
                ui.checkbox(&mut t.include_program, tl!("Include the program mix"));
                ui.end_row();
                ui.label(tl!("Write buffer"));
                ui.add(egui::DragValue::new(&mut t.buffer_mib).range(AudioTracksSettings::BUFFER_MIB).speed(4.0).suffix(format!(" {}", tl!("MiB"))));
                ui.end_row();
                ui.label(tl!("File size"));
                ui.add(egui::DragValue::new(&mut t.segment_gib).range(AudioTracksSettings::SEGMENT_GIB).suffix(format!(" {}", tl!("GiB"))));
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

    fn audio(&mut self, ui: &mut Ui) {
        let device = &mut self.draft.audio.monitoring_device;
        let listed = &self.output_devices;
        egui::Grid::new("audio-grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label(tl!("Monitoring device"));
            craft_ui::ComboBox::from_id_salt("monitoring-device").selected_text(device_name(device, listed)).width(320.0).show_ui(ui, |ui| {
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
        let v = &mut self.draft.video;
        let size_combo = |ui: &mut Ui, id: &str, w: &mut u32, h: &mut u32| {
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(w).range(16..=8192));
                ui.label("×");
                ui.add(egui::DragValue::new(h).range(16..=8192));
                craft_ui::ComboBox::from_id_salt(id).selected_text(tl!("Presets")).show_ui(ui, |ui| {
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
            let mut choices: Vec<(String, (u32, u32))> = Vec::with_capacity(self.displays.len() + 1);
            if let Some(s) = here {
                choices.push((tl!("The display this window is on").to_string(), s));
            }
            choices.extend(self.displays.iter().map(|d| (d.name.clone(), (d.width, d.height))));
            if !choices.is_empty() {
                ui.label(tl!("Match display"));
                craft_ui::ComboBox::from_id_salt("match-display").selected_text(tl!("Choose a display")).show_ui(ui, |ui| {
                    for (name, (w, h)) in &choices {
                        let fits = crate::canvas_for((*w, *h)).is_ok();
                        let on = v.base_width == *w && v.base_height == *h;
                        let (width, height) = (w.to_string(), h.to_string());
                        let label = craft_i18n::fmt(tl!("{name}: {width} × {height}"), &[("name", name), ("width", &width), ("height", &height)]);
                        if ui.add_enabled(fits, egui::Button::selectable(on, label)).clicked() {
                            (v.base_width, v.base_height) = (*w, *h);
                        }
                    }
                });
                ui.end_row();
            }
            ui.label(tl!("Output size"));
            size_combo(ui, "output-size", &mut v.output_width, &mut v.output_height);
            ui.end_row();
            ui.label(tl!("Frames per second"));
            let current = FPS
                .iter()
                .find(|(_, n, d)| *n == v.fps_num && *d == v.fps_den)
                .map_or_else(|| format!("{:.2}", v.fps()), |(l, _, _)| (*l).to_string());
            craft_ui::ComboBox::from_id_salt("fps").selected_text(current).show_ui(ui, |ui| {
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
                craft_ui::ComboBox::from_id_salt("graphics-adapter").selected_text(hardware::label(chosen, listed)).width(320.0).show_ui(ui, |ui| {
                    let mut choices = hardware::choices(listed);
                    if !choices.contains(chosen) {
                        choices.push(chosen.clone());
                    }
                    for c in choices {
                        let text = hardware::label(&c, listed);
                        ui.selectable_value(chosen, c, text);
                    }
                });
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
                craft_ui::ComboBox::from_id_salt("hardware-encoder").selected_text(hardware_encoder_label(encoder)).width(320.0).show_ui(ui, |ui| {
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
                Tab::Stream => self.stream(ui, registry),
                Tab::Output => self.output(ui, registry, &mut out),
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

/// The shown name of a Hardware encoder setting value.
fn hardware_encoder_label(id: &str) -> String {
    match craft_hwenc::Preference::parse(id) {
        craft_hwenc::Preference::Auto => tl!("Automatic (the first that passes a test)").to_string(),
        craft_hwenc::Preference::Software => tl!("Software only").to_string(),
        craft_hwenc::Preference::Named(id) => craft_hwenc::candidate(&id).map_or(id, |c| tl!(c.name).to_string()),
    }
}

/// The video encoders a recording (`streaming` false) or a stream can choose, each with whether
/// it can start: encoders the output cannot run are left out (a stream runs no built-in encoder,
/// and carries H.264 only), and ones the ffmpeg program does not list cannot start.
fn listed_encoders<'a>(registry: &'a Registry, offered: &[String], streaming: bool) -> Vec<(&'a dyn studiocraft_plugin::Encoder, bool)> {
    let env = EncoderEnv { builtin: !streaming, ..EncoderEnv::ffmpeg(offered) };
    registry
        .encoders()
        .iter()
        .filter(|e| e.kind() == EncoderKind::Video && (!streaming || e.streams()))
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
    fn recordings_list_the_built_in_encoder_and_streams_do_not() {
        let app = crate::StudioApp::boot(crate::store::Store::memory(), None, Vec::new());
        let registry = app.studio().registry();
        let offered = vec!["libx264".to_string()];
        let ids = |streaming| listed_encoders(registry, &offered, streaming).iter().map(|(e, ok)| (e.id(), *ok)).collect::<Vec<_>>();
        let recording = ids(false);
        assert!(recording.contains(&("h264", true)), "{recording:?}");
        assert!(recording.contains(&("x264", true)), "{recording:?}");
        let streaming = ids(true);
        assert!(!streaming.iter().any(|(id, _)| *id == "h264"), "{streaming:?}");
        assert!(streaming.contains(&("x264", true)), "{streaming:?}");
        // Without an ffmpeg program the built-in encoder can still record.
        assert!(listed_encoders(registry, &[], false).iter().any(|(e, ok)| e.id() == "h264" && *ok));
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
        assert_eq!(w.draft.audio_tracks, p.audio_tracks, "drawing the section changes nothing");
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
}
