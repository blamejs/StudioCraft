//! Saved files: scene collections and profiles as JSON in a versioned envelope,
//! `{"format": "...", "version": N, "data": {...}}`.
//!
//! Each format has a list of migrations, one per version step: `MIGRATIONS[n]` turns version
//! `n + 1` into version `n + 2`. Loading runs every step from the file's version up to the current
//! one before parsing. A file from a newer version is refused, so an older build never misreads or
//! overwrites it. When the current version goes up, add the step and a fixture of the old
//! version under `fixtures/`; the tests load every fixture.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use thiserror::Error;

use crate::collection::SceneCollection;
use crate::profile::Profile;

/// Why a file could not be read.
#[derive(Debug, Error, PartialEq)]
pub enum FormatError {
    #[error("the file is not valid JSON: {0}")]
    Json(String),
    #[error("the file is not a {expected} (it says `{found}`)")]
    WrongFormat { expected: &'static str, found: String },
    #[error("the file has no version number")]
    NoVersion,
    #[error("the file was written by a newer StudioCraft (format version {found}, this build reads up to {supported})")]
    TooNew { found: u32, supported: u32 },
    #[error("upgrading the file from version {from} failed: {reason}")]
    Migration { from: u32, reason: String },
    #[error("the file's contents don't fit: {0}")]
    Data(String),
}

/// One version step: changes the `data` value of version `n` into version `n + 1`.
pub type Migration = fn(&mut Value) -> Result<(), String>;

/// A versioned file format.
pub struct FileFormat {
    pub name: &'static str,
    pub version: u32,
    pub migrations: &'static [Migration],
}

/// Scene collections.
pub const COLLECTION: FileFormat = FileFormat { name: "studiocraft.scene-collection", version: 1, migrations: &[] };

/// Profiles.
pub const PROFILE: FileFormat = FileFormat { name: "studiocraft.profile", version: 1, migrations: &[] };

/// Largest file accepted, in bytes.
pub const MAX_FILE_BYTES: usize = 64 << 20;

impl FileFormat {
    /// `data` wrapped in this format's envelope, as pretty JSON. Text larger than
    /// [`MAX_FILE_BYTES`] is refused, since it could not be read back.
    pub fn write<T: Serialize>(&self, data: &T) -> Result<String, FormatError> {
        let data = serde_json::to_value(data).map_err(|e| FormatError::Data(e.to_string()))?;
        let text = serde_json::to_string_pretty(&json!({"format": self.name, "version": self.version, "data": data}))
            .map_err(|e| FormatError::Data(e.to_string()))?;
        if text.len() > MAX_FILE_BYTES {
            return Err(FormatError::Data(format!("the file would be larger than {} MB", MAX_FILE_BYTES >> 20)));
        }
        Ok(text)
    }

    /// The `data` of a file of this format at any version up to the current one, migrated to the
    /// current version.
    pub fn read_value(&self, text: &str) -> Result<Value, FormatError> {
        if text.len() > MAX_FILE_BYTES {
            return Err(FormatError::Data(format!("the file is larger than {} MB", MAX_FILE_BYTES >> 20)));
        }
        let mut root: Value = serde_json::from_str(text).map_err(|e| FormatError::Json(e.to_string()))?;
        let found = root.get("format").and_then(Value::as_str).unwrap_or_default();
        if found != self.name {
            return Err(FormatError::WrongFormat { expected: self.name, found: found.to_owned() });
        }
        let version = root.get("version").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()).ok_or(FormatError::NoVersion)?;
        if version == 0 {
            return Err(FormatError::NoVersion);
        }
        if version > self.version {
            return Err(FormatError::TooNew { found: version, supported: self.version });
        }
        let mut data = root.get_mut("data").map(Value::take).ok_or_else(|| FormatError::Data("missing `data`".into()))?;
        for from in version..self.version {
            let step = usize::try_from(from - 1)
                .ok()
                .and_then(|i| self.migrations.get(i))
                .ok_or(FormatError::Migration { from, reason: "no migration for this version".into() })?;
            step(&mut data).map_err(|reason| FormatError::Migration { from, reason })?;
        }
        Ok(data)
    }

    /// A file of this format parsed into `T` at the current version.
    pub fn read<T: DeserializeOwned>(&self, text: &str) -> Result<T, FormatError> {
        let data = self.read_value(text)?;
        serde_json::from_value(data).map_err(|e| FormatError::Data(e.to_string()))
    }
}

/// A scene collection file's text.
pub fn write_collection(c: &SceneCollection) -> Result<String, FormatError> {
    COLLECTION.write(c)
}

/// A scene collection from a file's text, migrated and repaired (see [`SceneCollection::repair`]).
pub fn read_collection(text: &str, default_scene_name: &str) -> Result<SceneCollection, FormatError> {
    let mut c: SceneCollection = COLLECTION.read(text)?;
    c.repair(default_scene_name);
    // A repair keeps every object, so an id used twice is still there and refuses the file.
    let problems = c.problems();
    if !problems.is_empty() {
        return Err(FormatError::Data(problems.join("; ")));
    }
    Ok(c)
}

/// A profile file's text.
pub fn write_profile(p: &Profile) -> Result<String, FormatError> {
    PROFILE.write(p)
}

/// A profile from a file's text, migrated, with video settings brought into range.
pub fn read_profile(text: &str) -> Result<Profile, FormatError> {
    let mut p: Profile = PROFILE.read(text)?;
    p.video = p.video.sanitized();
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every version ever written, oldest first: `(version, text)`. A new version adds a line.
    const COLLECTION_FIXTURES: &[(u32, &str)] = &[(1, include_str!("../fixtures/collection-v1.json"))];
    const PROFILE_FIXTURES: &[(u32, &str)] = &[(1, include_str!("../fixtures/profile-v1.json"))];

    fn covers_every_version(format: &FileFormat, fixtures: &[(u32, &str)]) {
        assert_eq!(format.migrations.len() as u32, format.version - 1, "{}: one migration per version step", format.name);
        let versions: Vec<u32> = fixtures.iter().map(|(v, _)| *v).collect();
        assert_eq!(versions, (1..=format.version).collect::<Vec<_>>(), "{}: one fixture per version", format.name);
    }

    #[test]
    fn every_collection_version_loads() {
        covers_every_version(&COLLECTION, COLLECTION_FIXTURES);
        for (v, text) in COLLECTION_FIXTURES {
            let c = read_collection(text, "Scene").unwrap_or_else(|e| panic!("version {v}: {e}"));
            assert!(c.problems().is_empty(), "version {v}: {:?}", c.problems());
            assert_eq!(c.scenes.len(), 2, "version {v}");
            assert_eq!(c.sources.len(), 3, "version {v}");
        }
    }

    #[test]
    fn every_profile_version_loads() {
        covers_every_version(&PROFILE, PROFILE_FIXTURES);
        for (v, text) in PROFILE_FIXTURES {
            let p = read_profile(text).unwrap_or_else(|e| panic!("version {v}: {e}"));
            assert_eq!(p.video.base_width, 1280, "version {v}");
        }
    }

    #[test]
    fn collection_round_trips() {
        let (_, text) = COLLECTION_FIXTURES[0];
        let c = read_collection(text, "Scene").unwrap();
        let again = read_collection(&write_collection(&c).unwrap(), "Scene").unwrap();
        assert_eq!(c, again);
    }

    #[test]
    fn a_collection_with_an_id_used_twice_is_refused() {
        let (_, text) = COLLECTION_FIXTURES[0];
        let mut c = read_collection(text, "Scene").unwrap();
        let mut copy = c.scenes[0].clone();
        copy.name = "Copy".into();
        c.scenes.push(copy);
        let e = read_collection(&write_collection(&c).unwrap(), "Scene").unwrap_err();
        assert!(matches!(&e, FormatError::Data(m) if m.contains("used twice")), "{e}");
    }

    #[test]
    fn newer_and_foreign_files_are_refused() {
        let newer = format!(r#"{{"format":"{}","version":{},"data":{{}}}}"#, COLLECTION.name, COLLECTION.version + 1);
        assert!(matches!(read_collection(&newer, "Scene"), Err(FormatError::TooNew { .. })));
        assert!(matches!(read_collection(r#"{"format":"other","version":1,"data":{}}"#, "Scene"), Err(FormatError::WrongFormat { .. })));
        assert!(matches!(read_collection("{", "Scene"), Err(FormatError::Json(_))));
        assert!(matches!(
            read_collection(r#"{"format":"studiocraft.scene-collection","version":0,"data":{}}"#, "Scene"),
            Err(FormatError::NoVersion)
        ));
        assert!(matches!(read_collection(r#"{"format":"studiocraft.scene-collection","version":1,"data":[]}"#, "Scene"), Err(FormatError::Data(_))));
    }

    #[test]
    fn migrations_run_in_order() {
        fn add_a(v: &mut Value) -> Result<(), String> {
            v["steps"].as_array_mut().ok_or("no steps")?.push(json!("a"));
            Ok(())
        }
        fn add_b(v: &mut Value) -> Result<(), String> {
            v["steps"].as_array_mut().ok_or("no steps")?.push(json!("b"));
            Ok(())
        }
        const F: FileFormat = FileFormat { name: "test", version: 3, migrations: &[add_a, add_b] };
        let v1 = F.read_value(r#"{"format":"test","version":1,"data":{"steps":[]}}"#).unwrap();
        assert_eq!(v1["steps"], json!(["a", "b"]));
        let v2 = F.read_value(r#"{"format":"test","version":2,"data":{"steps":[]}}"#).unwrap();
        assert_eq!(v2["steps"], json!(["b"]));
        let broken = F.read_value(r#"{"format":"test","version":1,"data":{}}"#);
        assert!(matches!(broken, Err(FormatError::Migration { from: 1, .. })));
    }
}
