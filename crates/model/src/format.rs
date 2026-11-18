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
pub const COLLECTION: FileFormat =
    FileFormat { name: "studiocraft.scene-collection", version: 4, migrations: &[collection_v1_to_v2, collection_v2_to_v3, collection_v3_to_v4] };

/// Collection version 1 to 2: each source's audio has a sync offset (`sync_offset_ms`), 0 for the
/// sources of an older file.
fn collection_v1_to_v2(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the collection is not an object")?;
    if let Some(Value::Array(sources)) = data.get_mut("sources") {
        for source in sources.iter_mut().filter_map(Value::as_object_mut) {
            let audio = source.entry("audio").or_insert_with(|| json!({}));
            if let Value::Object(a) = audio {
                a.entry("sync_offset_ms").or_insert_with(|| json!(0));
            }
        }
    }
    Ok(())
}

/// Collection version 2 to 3: each source's audio has a monitoring mode (`monitor`), `off` for
/// the sources of an older file.
fn collection_v2_to_v3(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the collection is not an object")?;
    if let Some(Value::Array(sources)) = data.get_mut("sources") {
        for source in sources.iter_mut().filter_map(Value::as_object_mut) {
            let audio = source.entry("audio").or_insert_with(|| json!({}));
            if let Value::Object(a) = audio {
                a.entry("monitor").or_insert_with(|| json!("off"));
            }
        }
    }
    Ok(())
}

/// Collection version 3 to 4: each source's audio says whether audio tracks record it to a file
/// of its own (`record_track`); `null` for the sources of an older file, which records the
/// sources heard in the program.
fn collection_v3_to_v4(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the collection is not an object")?;
    if let Some(Value::Array(sources)) = data.get_mut("sources") {
        for source in sources.iter_mut().filter_map(Value::as_object_mut) {
            let audio = source.entry("audio").or_insert_with(|| json!({}));
            if let Value::Object(a) = audio {
                a.entry("record_track").or_insert(Value::Null);
            }
        }
    }
    Ok(())
}

/// Profiles.
pub const PROFILE: FileFormat = FileFormat {
    name: "studiocraft.profile",
    version: 6,
    migrations: &[profile_v1_to_v2, profile_v2_to_v3, profile_v3_to_v4, profile_v4_to_v5, profile_v5_to_v6],
};

/// Profile version 1 to 2: the recording and the stream each keep the settings of their video
/// encoders (`encoder_settings`, empty until the user changes one).
fn profile_v1_to_v2(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the profile is not an object")?;
    for section in ["output", "stream"] {
        if let Some(Value::Object(s)) = data.get_mut(section) {
            s.entry("encoder_settings").or_insert_with(|| json!({}));
        }
    }
    Ok(())
}

/// Profile version 2 to 3: the replay buffer's length and memory limit (`replay`), at 20 seconds
/// and 512 MB.
fn profile_v2_to_v3(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the profile is not an object")?;
    data.entry("replay").or_insert_with(|| json!({"seconds": 20, "memory_mb": 512}));
    Ok(())
}

/// Profile version 3 to 4: monitored sources play on the system's default output
/// (`audio.monitoring_device`, `default`).
fn profile_v3_to_v4(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the profile is not an object")?;
    let audio = data.entry("audio").or_insert_with(|| json!({}));
    if let Value::Object(a) = audio {
        a.entry("monitoring_device").or_insert_with(|| json!(crate::profile::DEFAULT_AUDIO_DEVICE));
    }
    Ok(())
}

/// Profile version 4 to 5: the Hardware video encoder chooses automatically
/// (`output.hardware_encoder`, `auto`).
fn profile_v4_to_v5(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the profile is not an object")?;
    let output = data.entry("output").or_insert_with(|| json!({}));
    if let Value::Object(o) = output {
        o.entry("hardware_encoder").or_insert_with(|| json!("auto"));
    }
    Ok(())
}

/// Profile version 5 to 6: audio tracks (`audio_tracks`), off, recorded while a recording runs,
/// with the program mix, a 96 MiB buffer and 2 GiB files.
fn profile_v5_to_v6(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the profile is not an object")?;
    data.entry("audio_tracks")
        .or_insert_with(|| json!({"enabled": false, "when": "recording", "include_program": true, "buffer_mib": 96, "segment_gib": 2}));
    Ok(())
}

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

/// A profile from a file's text, migrated, with video and replay buffer settings brought into
/// range. Audio tracks settings out of range refuse the file.
pub fn read_profile(text: &str) -> Result<Profile, FormatError> {
    let mut p: Profile = PROFILE.read(text)?;
    p.video = p.video.sanitized();
    p.replay = p.replay.sanitized();
    p.audio_tracks.check().map_err(FormatError::Data)?;
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every version ever written, oldest first: `(version, text)`. A new version adds a line.
    const COLLECTION_FIXTURES: &[(u32, &str)] = &[
        (1, include_str!("../fixtures/collection-v1.json")),
        (2, include_str!("../fixtures/collection-v2.json")),
        (3, include_str!("../fixtures/collection-v3.json")),
        (4, include_str!("../fixtures/collection-v4.json")),
    ];
    const PROFILE_FIXTURES: &[(u32, &str)] = &[
        (1, include_str!("../fixtures/profile-v1.json")),
        (2, include_str!("../fixtures/profile-v2.json")),
        (3, include_str!("../fixtures/profile-v3.json")),
        (4, include_str!("../fixtures/profile-v4.json")),
        (5, include_str!("../fixtures/profile-v5.json")),
        (6, include_str!("../fixtures/profile-v6.json")),
    ];

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
    fn profiles_keep_the_settings_of_each_encoder() {
        let (_, v1) = PROFILE_FIXTURES[0];
        let old = read_profile(v1).unwrap();
        assert!(old.output.encoder_settings.is_empty() && old.stream.encoder_settings.is_empty());
        assert_eq!(old.output.video_encoder_settings(), crate::Settings::new(), "a version 1 profile keeps the encoders' defaults");

        let (_, v2) = PROFILE_FIXTURES[1];
        let p = read_profile(v2).unwrap();
        assert_eq!(p.output.video_encoder_settings().str("preset"), Some("fast"));
        assert_eq!(p.stream.video_encoder_settings().str("preset"), Some("p4"));
        let mut other = p.clone();
        other.output.video_encoder = "nvenc_h264".into();
        assert_eq!(other.output.video_encoder_settings(), crate::Settings::new(), "another encoder does not take x264's preset");
        assert_eq!(read_profile(&write_profile(&p).unwrap()).unwrap(), p);
    }

    #[test]
    fn profiles_keep_the_replay_buffer_settings() {
        for (v, text) in &PROFILE_FIXTURES[..2] {
            let p = read_profile(text).unwrap();
            assert_eq!(p.replay, crate::ReplaySettings { seconds: 20, memory_mb: 512 }, "version {v} gets the defaults");
        }
        let (_, v3) = PROFILE_FIXTURES[2];
        let p = read_profile(v3).unwrap();
        assert_eq!(p.replay, crate::ReplaySettings { seconds: 45, memory_mb: 1024 });
        assert_eq!(read_profile(&write_profile(&p).unwrap()).unwrap(), p);
        let wild = v3.replace("\"seconds\": 45", "\"seconds\": 1").replace("\"memory_mb\": 1024", "\"memory_mb\": 999999");
        let p = read_profile(&wild).unwrap();
        assert_eq!(p.replay, crate::ReplaySettings { seconds: 5, memory_mb: 16_384 }, "values out of range are brought into it");
    }

    #[test]
    fn collections_keep_each_source_sync_offset() {
        let (_, v1) = COLLECTION_FIXTURES[0];
        let old = read_collection(v1, "Scene").unwrap();
        assert!(old.sources.iter().all(|s| s.audio.sync_offset_ms == 0), "a version 1 collection has no offsets");
        let (_, v2) = COLLECTION_FIXTURES[1];
        let c = read_collection(v2, "Scene").unwrap();
        assert_eq!(c.source_by_name("Microphone").map(|s| s.audio.sync_offset_ms), Some(250));
        assert_eq!(read_collection(&write_collection(&c).unwrap(), "Scene").unwrap(), c);
        let wild = v2.replace("\"sync_offset_ms\": 250", "\"sync_offset_ms\": 4000000");
        let c = read_collection(&wild, "Scene").unwrap();
        assert_eq!(
            c.source_by_name("Microphone").map(|s| s.audio.sync_offset_ms),
            Some(crate::collection::MAX_SYNC_OFFSET_MS),
            "an offset out of range is brought into it"
        );
        let negative = v2.replace("\"sync_offset_ms\": 250", "\"sync_offset_ms\": -5");
        assert!(matches!(read_collection(&negative, "Scene"), Err(FormatError::Data(_))));
    }

    #[test]
    fn collections_keep_each_source_monitoring_mode() {
        for (v, text) in &COLLECTION_FIXTURES[..2] {
            let c = read_collection(text, "Scene").unwrap();
            assert!(c.sources.iter().all(|s| s.audio.monitor == crate::MonitorMode::Off), "version {v} monitors nothing");
        }
        let (_, v3) = COLLECTION_FIXTURES[2];
        let c = read_collection(v3, "Scene").unwrap();
        assert_eq!(c.source_by_name("Microphone").map(|s| s.audio.monitor), Some(crate::MonitorMode::MonitorOnly));
        assert_eq!(read_collection(&write_collection(&c).unwrap(), "Scene").unwrap(), c);
        for mode in crate::MonitorMode::ALL {
            let text = v3.replace("\"monitor_only\"", &format!("\"{}\"", mode.id()));
            assert_eq!(read_collection(&text, "Scene").unwrap().source_by_name("Microphone").map(|s| s.audio.monitor), Some(mode));
        }
        let unknown = v3.replace("\"monitor_only\"", "\"loud\"");
        assert!(matches!(read_collection(&unknown, "Scene"), Err(FormatError::Data(_))), "an unknown mode refuses the file");
    }

    #[test]
    fn profiles_keep_the_monitoring_device() {
        for (v, text) in &PROFILE_FIXTURES[..3] {
            let p = read_profile(text).unwrap();
            assert_eq!(p.audio.monitoring_device, crate::profile::DEFAULT_AUDIO_DEVICE, "version {v} plays on the default output");
        }
        let (_, v4) = PROFILE_FIXTURES[3];
        let p = read_profile(v4).unwrap();
        assert_eq!(p.audio.monitoring_device, "Headphones (USB)");
        assert_eq!(read_profile(&write_profile(&p).unwrap()).unwrap(), p);
        let omitted = v4.replace(", \"monitoring_device\": \"Headphones (USB)\"", "");
        assert_eq!(read_profile(&omitted).unwrap().audio.monitoring_device, crate::profile::DEFAULT_AUDIO_DEVICE, "an omitted device is the default");
        let wrong = v4.replace("\"Headphones (USB)\"", "7");
        assert!(matches!(read_profile(&wrong), Err(FormatError::Data(_))), "a device that is not a name refuses the file");
    }

    #[test]
    fn profiles_keep_the_hardware_encoder() {
        for (v, text) in &PROFILE_FIXTURES[..4] {
            let p = read_profile(text).unwrap();
            assert_eq!(p.output.hardware_encoder, "auto", "version {v} chooses automatically");
        }
        let (_, v5) = PROFILE_FIXTURES[4];
        let p = read_profile(v5).unwrap();
        assert_eq!(p.output.hardware_encoder, "nvenc_hevc");
        assert_eq!(read_profile(&write_profile(&p).unwrap()).unwrap(), p);
    }

    #[test]
    fn collections_keep_whether_each_source_records_its_own_track() {
        for (v, text) in &COLLECTION_FIXTURES[..3] {
            let c = read_collection(text, "Scene").unwrap();
            assert!(c.sources.iter().all(|s| s.audio.record_track.is_none()), "version {v} follows the program");
            let mic = c.source_by_name("Microphone").map(|s| s.audio.records_track());
            assert_eq!(mic, if *v < 3 { Some(true) } else { Some(false) }, "version {v}: a monitor-only source is left out");
        }
        let (_, v4) = COLLECTION_FIXTURES[3];
        let c = read_collection(v4, "Scene").unwrap();
        let flag = |name: &str| c.source_by_name(name).map(|s| (s.audio.record_track, s.audio.records_track()));
        assert_eq!(flag("Background"), Some((None, true)));
        assert_eq!(flag("Title"), Some((Some(false), false)));
        assert_eq!(flag("Microphone"), Some((Some(true), true)), "set on, a monitor-only source is recorded");
        assert_eq!(read_collection(&write_collection(&c).unwrap(), "Scene").unwrap(), c);
        let wrong = v4.replace("\"record_track\": true", "\"record_track\": \"yes\"");
        assert!(matches!(read_collection(&wrong, "Scene"), Err(FormatError::Data(_))), "a flag that is not a boolean refuses the file");
    }

    #[test]
    fn profiles_keep_the_audio_tracks_settings_and_refuse_values_out_of_range() {
        for (v, text) in &PROFILE_FIXTURES[..5] {
            let p = read_profile(text).unwrap();
            assert_eq!(p.audio_tracks, crate::profile::AudioTracksSettings::default(), "version {v} gets the defaults");
            assert!(!p.audio_tracks.enabled, "version {v}: audio tracks are off");
        }
        let (_, v6) = PROFILE_FIXTURES[5];
        let p = read_profile(v6).unwrap();
        let want = crate::profile::AudioTracksSettings {
            enabled: true,
            when: crate::profile::AudioTracksWhen::Either,
            include_program: false,
            buffer_mib: 256,
            segment_gib: 1,
        };
        assert_eq!(p.audio_tracks, want);
        assert_eq!(read_profile(&write_profile(&p).unwrap()).unwrap(), p);
        for (from, to) in [
            ("\"buffer_mib\": 256", "\"buffer_mib\": 15"),
            ("\"buffer_mib\": 256", "\"buffer_mib\": 1025"),
            ("\"segment_gib\": 1", "\"segment_gib\": 0"),
            ("\"segment_gib\": 1", "\"segment_gib\": 4"),
            ("\"either\"", "\"sometimes\""),
            ("\"segment_gib\": 1", "\"segment_gib\": -1"),
        ] {
            let bad = v6.replace(from, to);
            assert!(matches!(read_profile(&bad), Err(FormatError::Data(_))), "{to} refuses the file");
        }
        for (from, to) in [("\"buffer_mib\": 256", "\"buffer_mib\": 16"), ("\"segment_gib\": 1", "\"segment_gib\": 3")] {
            assert!(read_profile(&v6.replace(from, to)).is_ok(), "{to} is in range");
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
        let current = format!(r#"{{"format":"{}","version":{},"data":[]}}"#, COLLECTION.name, COLLECTION.version);
        assert!(matches!(read_collection(&current, "Scene"), Err(FormatError::Data(_))));
        assert!(matches!(
            read_collection(r#"{"format":"studiocraft.scene-collection","version":1,"data":[]}"#, "Scene"),
            Err(FormatError::Migration { from: 1, .. })
        ));
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
