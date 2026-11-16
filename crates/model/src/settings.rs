//! A plug-in's settings: a JSON object keyed by the field keys of its [`crate::SettingsSchema`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Settings values by key.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Settings(pub BTreeMap<String, Value>);

impl Settings {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: `key` set to `value`.
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.set(key, value);
        self
    }

    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.0.insert(key.to_owned(), value.into());
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn bool(&self, key: &str) -> Option<bool> {
        self.0.get(key).and_then(Value::as_bool)
    }

    pub fn i64(&self, key: &str) -> Option<i64> {
        self.0.get(key).and_then(Value::as_i64)
    }

    pub fn f64(&self, key: &str) -> Option<f64> {
        self.0.get(key).and_then(Value::as_f64).filter(|v| v.is_finite())
    }

    pub fn str(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(Value::as_str)
    }

    /// A color stored as `#rrggbb` or `#rrggbbaa`, as `[r, g, b, a]`.
    pub fn color(&self, key: &str) -> Option<[u8; 4]> {
        self.str(key).and_then(parse_color)
    }

    /// Settings from a JSON object; anything else is an error.
    pub fn from_json(value: &Value) -> Result<Settings, String> {
        match value {
            Value::Object(map) => Ok(Settings(map.iter().map(|(k, v)| (k.clone(), v.clone())).collect())),
            Value::Null => Ok(Settings::default()),
            _ => Err("settings must be a JSON object".into()),
        }
    }

    pub fn to_json(&self) -> Value {
        Value::Object(self.0.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
    }
}

/// `#rrggbb` or `#rrggbbaa` (the `#` optional) as `[r, g, b, a]`.
pub fn parse_color(text: &str) -> Option<[u8; 4]> {
    let hex = text.trim().trim_start_matches('#');
    if !(hex.len() == 6 || hex.len() == 8) || !hex.is_ascii() {
        return None;
    }
    let byte = |i: usize| hex.get(i..i + 2).and_then(|s| u8::from_str_radix(s, 16).ok());
    let a = if hex.len() == 8 { byte(6)? } else { 255 };
    Some([byte(0)?, byte(2)?, byte(4)?, a])
}

/// `[r, g, b, a]` as `#rrggbbaa`.
pub fn format_color(c: [u8; 4]) -> String {
    format!("#{:02x}{:02x}{:02x}{:02x}", c[0], c[1], c[2], c[3])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors_round_trip() {
        assert_eq!(parse_color("#ff8000"), Some([255, 128, 0, 255]));
        assert_eq!(parse_color("10203040"), Some([16, 32, 48, 64]));
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color("#gg0000"), None);
        assert_eq!(parse_color("#ééé"), None);
        assert_eq!(format_color([1, 2, 3, 4]), "#01020304");
    }

    #[test]
    fn typed_getters_reject_wrong_types() {
        let s = Settings::new().with("n", 3).with("t", "x").with("f", 1.5);
        assert_eq!(s.i64("n"), Some(3));
        assert_eq!(s.str("n"), None);
        assert_eq!(s.f64("f"), Some(1.5));
        assert_eq!(s.bool("t"), None);
        assert!(Settings::from_json(&serde_json::json!([1])).is_err());
    }
}
