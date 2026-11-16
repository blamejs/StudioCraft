//! Settings schemas. Every plug-in (source, filter, transition, encoder, output) describes its
//! settings with a [`SettingsSchema`]; the properties window is built from it, and settings
//! coming from files, scripts or the control channel are normalized against it.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::settings::{Settings, parse_color};

/// How a path field picks its path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathMode {
    OpenFile,
    SaveFile,
    Folder,
}

/// One entry of a choice field.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChoiceOption {
    pub value: Value,
    /// English label; the interface translates it.
    pub label: String,
}

impl ChoiceOption {
    pub fn new(value: impl Into<Value>, label: impl Into<String>) -> Self {
        Self { value: value.into(), label: label.into() }
    }
}

/// The kind of value a field holds, with its limits.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FieldKind {
    Bool,
    Int {
        min: i64,
        max: i64,
    },
    Float {
        min: f64,
        max: f64,
        step: f64,
    },
    Text {
        multiline: bool,
    },
    /// `#rrggbbaa`.
    Color,
    Path {
        mode: PathMode,
        filter_name: String,
        extensions: Vec<String>,
    },
    Choice {
        options: Vec<ChoiceOption>,
    },
    /// A font family name.
    Font,
}

/// One setting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub key: String,
    /// English label; the interface translates it.
    pub label: String,
    pub kind: FieldKind,
    pub default: Value,
}

impl Field {
    pub fn new(key: &str, label: &str, kind: FieldKind, default: impl Into<Value>) -> Self {
        Self { key: key.into(), label: label.into(), kind, default: default.into() }
    }

    /// Whether `v` is a valid value for this field, after [`Field::coerce`].
    fn accepts(&self, v: &Value) -> bool {
        match &self.kind {
            FieldKind::Bool => v.is_boolean(),
            FieldKind::Int { min, max } => v.as_i64().is_some_and(|n| (*min..=*max).contains(&n)),
            FieldKind::Float { min, max, .. } => v.as_f64().is_some_and(|n| n.is_finite() && n >= *min && n <= *max),
            FieldKind::Text { .. } | FieldKind::Font => v.is_string(),
            FieldKind::Path { .. } => v.is_string(),
            FieldKind::Color => v.as_str().is_some_and(|s| parse_color(s).is_some()),
            // Choices filled at run time (devices, windows) may not list a saved value any more:
            // keep any value of a JSON scalar type, so reconnecting the device restores it.
            FieldKind::Choice { .. } => v.is_string() || v.is_number() || v.is_boolean(),
        }
    }

    /// Whether `v` is a valid new value for this field: as [`Field::accepts`], and a choice must
    /// be one of the options listed now or the default.
    fn accepts_new(&self, v: &Value) -> bool {
        match &self.kind {
            FieldKind::Choice { options } => *v == self.default || options.iter().any(|o| o.value == *v),
            _ => self.accepts(v),
        }
    }

    /// `v` brought into range where that keeps its meaning: numbers are clamped.
    fn coerce(&self, v: &Value) -> Value {
        match &self.kind {
            FieldKind::Int { min, max } => match v.as_f64() {
                Some(n) if n.is_finite() => Value::from((n.round() as i64).clamp(*min, *max)),
                _ => v.clone(),
            },
            FieldKind::Float { min, max, .. } => match v.as_f64() {
                Some(n) if n.is_finite() => serde_json::Number::from_f64(n.clamp(*min, *max)).map_or_else(|| v.clone(), Value::Number),
                _ => v.clone(),
            },
            _ => v.clone(),
        }
    }
}

/// A plug-in's settings: its fields, in display order.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SettingsSchema {
    pub fields: Vec<Field>,
}

impl SettingsSchema {
    pub fn new(fields: Vec<Field>) -> Self {
        Self { fields }
    }

    pub fn field(&self, key: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.key == key)
    }

    /// The defaults of every field.
    pub fn defaults(&self) -> Settings {
        let mut s = Settings::default();
        for f in &self.fields {
            s.set(&f.key, f.default.clone());
        }
        s
    }

    /// `settings` completed and corrected: missing or invalid values take the field's default,
    /// numbers are clamped into range, and keys the schema doesn't know are kept unchanged (a
    /// newer version of the plug-in may read them).
    pub fn normalize(&self, settings: &Settings) -> Settings {
        let mut out = settings.clone();
        for f in &self.fields {
            let v = match settings.get(&f.key) {
                Some(v) => {
                    let c = f.coerce(v);
                    if f.accepts(&c) { c } else { f.default.clone() }
                }
                None => f.default.clone(),
            };
            out.set(&f.key, v);
        }
        out
    }

    /// The problems with new values in `settings`, one line each: unknown keys are not problems,
    /// and a choice must be one of the options listed now.
    pub fn problems(&self, settings: &Settings) -> Vec<String> {
        self.fields
            .iter()
            .filter_map(|f| match settings.get(&f.key) {
                Some(v) if !f.accepts_new(v) => Some(format!("`{}` has an invalid value {v}", f.key)),
                _ => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn schema() -> SettingsSchema {
        SettingsSchema::new(vec![
            Field::new("w", "Width", FieldKind::Int { min: 1, max: 100 }, 10),
            Field::new("o", "Opacity", FieldKind::Float { min: 0.0, max: 1.0, step: 0.01 }, 1.0),
            Field::new("c", "Color", FieldKind::Color, "#ffffffff"),
            Field::new("m", "Mode", FieldKind::Choice { options: vec![ChoiceOption::new("a", "A")] }, "a"),
        ])
    }

    #[test]
    fn normalize_fills_clamps_and_keeps_unknown_keys() {
        let s = Settings::new().with("w", 500).with("o", -3.0).with("c", "nope").with("extra", true);
        let n = schema().normalize(&s);
        assert_eq!(n.i64("w"), Some(100));
        assert_eq!(n.f64("o"), Some(0.0));
        assert_eq!(n.str("c"), Some("#ffffffff"));
        assert_eq!(n.str("m"), Some("a"));
        assert_eq!(n.bool("extra"), Some(true));
        assert!(schema().problems(&n).is_empty());
    }

    #[test]
    fn problems_name_the_field() {
        let s = Settings::new().with("w", "wide");
        let p = schema().problems(&s);
        assert_eq!(p.len(), 1);
        assert!(p[0].contains("`w`"));
        assert_eq!(schema().normalize(&s).i64("w"), Some(10));
    }

    #[test]
    fn a_new_choice_must_be_listed_and_a_saved_one_is_kept() {
        let unlisted = Settings::new().with("m", "unplugged");
        assert_eq!(schema().problems(&unlisted).len(), 1);
        assert!(schema().problems(&Settings::new().with("m", "a")).is_empty());
        // A saved device that is not connected now keeps its value.
        assert_eq!(schema().normalize(&unlisted).str("m"), Some("unplugged"));
    }

    #[test]
    fn schema_serializes_for_scripts() {
        let v = serde_json::to_value(schema()).unwrap();
        assert_eq!(v["fields"][0]["kind"], json!({"type": "int", "min": 1, "max": 100}));
    }
}
