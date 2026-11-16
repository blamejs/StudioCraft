//! Forms built from a plug-in's [`SettingsSchema`], and the Properties and Filters windows.

use egui::{Color32, Ui};
use serde_json::{Value, json};
use studiocraft_model::{FieldKind, FilterId, PathMode, SettingsSchema, SourceId};
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

fn color_text(c: [u8; 4]) -> String {
    format!("#{:02x}{:02x}{:02x}{:02x}", c[0], c[1], c[2], c[3])
}

/// Edit `settings` against `schema`; the values the schema does not list are kept.
pub fn form(ui: &mut Ui, id: egui::Id, schema: &SettingsSchema, settings: &mut Settings) -> FormEvent {
    let mut ev = FormEvent::default();
    egui::Grid::new(id).num_columns(2).spacing([12.0, 6.0]).striped(false).show(ui, |ui| {
        for field in &schema.fields {
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
                    let selected = options.iter().find(|o| o.value == current).map_or_else(|| current.to_string(), |o| tl!(&o.label).to_string());
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

/// The Properties window of one source.
pub struct Properties {
    pub source: SourceId,
    /// Read when the window opened (lists of displays, windows and devices are read then).
    schema: SettingsSchema,
    settings: Settings,
}

impl Properties {
    pub fn open(studio: &Studio, source: SourceId) -> Option<Properties> {
        let s = studio.collection().source(source)?;
        let schema = studio.registry().source(&s.kind).ok()?.schema();
        Some(Properties { source, schema, settings: s.settings.clone() })
    }

    /// Point a path field at a chosen file.
    pub fn set(&mut self, key: &str, value: &str) -> Value {
        self.settings.set(key, value);
        self.command()
    }

    fn command(&self) -> Value {
        json!({"source": self.source, "settings": self.settings})
    }

    /// Draw the window; the `source.settings` parameters to run when a value changed, and a
    /// requested file dialog. `open` turns false when the window is closed.
    pub fn show(&mut self, ctx: &egui::Context, studio: &Studio, open: &mut bool) -> (Option<Value>, Option<Browse>) {
        let Some(src) = studio.collection().source(self.source) else {
            *open = false;
            return (None, None);
        };
        if src.settings != self.settings && !ctx.egui_is_using_pointer() && !ctx.egui_wants_keyboard_input() {
            // Undo, a script or the control channel changed them.
            self.settings = src.settings.clone();
        }
        let title = craft_i18n::fmt(tl!("Properties for '{name}'"), &[("name", &src.name)]);
        let error = studio.source_error(self.source);
        let mut out = (None, None);
        let mut refresh = false;
        let window = egui::Window::new(title).id(egui::Id::unique(("properties", self.source.0))).open(open).resizable(true).default_width(440.0);
        window.show(ctx, |ui| {
            if let Some(e) = &error {
                ui.colored_label(ui.visuals().error_fg_color, crate::i18n::Msg::plain(e.to_string()).shown());
                ui.separator();
            }
            egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
                let ev = form(ui, egui::Id::unique(("props-form", self.source.0)), &self.schema, &mut self.settings);
                if ev.changed {
                    out.0 = Some(self.command());
                }
                out.1 = ev.browse;
            });
            ui.separator();
            ui.horizontal(|ui| {
                if ui.button(tl!("Defaults")).clicked() {
                    self.settings = self.schema.defaults();
                    out.0 = Some(self.command());
                }
                if ui.button(tl!("Refresh Lists")).on_hover_text(tl!("Read the displays, windows and devices again")).clicked() {
                    refresh = true;
                }
            });
        });
        if refresh && let Ok(p) = studio.registry().source(&src.kind) {
            self.schema = p.schema();
        }
        out
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
    fn colors_are_written_as_hex_with_alpha() {
        assert_eq!(color_text([255, 16, 0, 128]), "#ff100080");
    }
}
