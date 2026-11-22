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
pub const COLLECTION: FileFormat = FileFormat {
    name: "studiocraft.scene-collection",
    version: 5,
    migrations: &[collection_v1_to_v2, collection_v2_to_v3, collection_v3_to_v4, collection_v4_to_v5],
};

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

/// Collection version 4 to 5: each scene item can keep how it was sized to the canvas (`fit`:
/// `"fit"`, `"fill"` or `"stretch"`). The items of an older file get `null`, and a new canvas size
/// keeps them in proportion.
fn collection_v4_to_v5(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the collection is not an object")?;
    if let Some(Value::Array(scenes)) = data.get_mut("scenes") {
        for scene in scenes.iter_mut().filter_map(Value::as_object_mut) {
            if let Some(Value::Array(items)) = scene.get_mut("items") {
                for item in items.iter_mut().filter_map(Value::as_object_mut) {
                    item.entry("fit").or_insert(Value::Null);
                }
            }
        }
    }
    Ok(())
}

/// Profiles.
pub const PROFILE: FileFormat = FileFormat {
    name: "studiocraft.profile",
    version: 12,
    migrations: &[
        profile_v1_to_v2,
        profile_v2_to_v3,
        profile_v3_to_v4,
        profile_v4_to_v5,
        profile_v5_to_v6,
        profile_v6_to_v7,
        profile_v7_to_v8,
        profile_v8_to_v9,
        profile_v9_to_v10,
        profile_v10_to_v11,
        profile_v11_to_v12,
    ],
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
/// with the program mix, a 96 MiB buffer and 1 GiB files.
fn profile_v5_to_v6(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the profile is not an object")?;
    data.entry("audio_tracks")
        .or_insert_with(|| json!({"enabled": false, "when": "recording", "include_program": true, "buffer_mib": 96, "segment_gib": 1}));
    Ok(())
}

/// Profile version 6 to 7: the stream has a list of destinations (`stream.destinations`). The
/// server of an older file becomes one destination, `main`, which is on and uses the profile's
/// key (`stream.key`, kept as it was); a file without a server gets no destination. The stream
/// runs the built-in encoders, so a stream encoder that ran inside ffmpeg is replaced by the
/// built-in encoder of the same kind: `x264` by `h264`, and NVENC, Media Foundation,
/// VideoToolbox and VA-API H.264 by their built-in forms, and AMF and Quick Sync H.264, which have
/// none, by the hardware choice (`hardware`). Other encoders are kept, and a stream that starts
/// with one asks for another.
fn profile_v6_to_v7(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the profile is not an object")?;
    let stream = data.entry("stream").or_insert_with(|| json!({}));
    let Value::Object(s) = stream else { return Err("the stream settings are not an object".into()) };
    let server = match s.remove("server") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(server)) => server,
        Some(_) => return Err("the stream server is not text".into()),
    };
    let destinations = if server.trim().is_empty() {
        json!([])
    } else {
        json!([{"id": crate::profile::PRIMARY_DESTINATION, "name": "", "enabled": true, "server": server, "key": "profile"}])
    };
    s.insert("destinations".into(), destinations);
    if let Some(Value::String(encoder)) = s.get_mut("video_encoder") {
        let built_in = match encoder.as_str() {
            "x264" => Some("h264"),
            "nvenc_h264" => Some("nvenc_native_h264"),
            "mf_h264" => Some("mf_native_h264"),
            "vt_h264" => Some("vt_native_h264"),
            "vaapi_h264" => Some("vaapi_native_h264"),
            // These run only inside ffmpeg; the hardware choice picks one that runs here.
            "amf_h264" | "qsv_h264" => Some("hardware"),
            _ => None,
        };
        if let Some(id) = built_in {
            *encoder = id.to_string();
        }
    }
    Ok(())
}

/// Profile version 7 to 8: a take of audio tracks that ends does nothing more
/// (`audio_tracks.after_stop`, `nothing`), and the audio editor is looked up on the `PATH`
/// (`audio_tracks.editor_path`, empty).
fn profile_v7_to_v8(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the profile is not an object")?;
    let tracks = data.entry("audio_tracks").or_insert_with(|| json!({}));
    if let Value::Object(t) = tracks {
        t.entry("after_stop").or_insert_with(|| json!("nothing"));
        t.entry("editor_path").or_insert_with(|| json!(""));
    }
    Ok(())
}

/// Profile version 8 to 9: each stream destination names the streaming service it was set up for
/// (`service`); an older file's destinations are servers of the user's own choosing (empty).
fn profile_v8_to_v9(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the profile is not an object")?;
    if let Some(Value::Object(s)) = data.get_mut("stream")
        && let Some(Value::Array(destinations)) = s.get_mut("destinations")
    {
        for d in destinations.iter_mut().filter_map(Value::as_object_mut) {
            d.entry("service").or_insert_with(|| json!(""));
        }
    }
    Ok(())
}

/// Profile version 9 to 10: the pause screen (`pause`): no media on a `#101014ff` background, a
/// 300 second countdown reading "Back in {time}" at the bottom center, which keeps the screen at
/// zero, and the program mix held while paused.
fn profile_v9_to_v10(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the profile is not an object")?;
    let defaults = serde_json::to_value(crate::PauseSettings::default()).map_err(|e| e.to_string())?;
    data.entry("pause").or_insert(defaults);
    Ok(())
}

/// Profile version 10 to 11: the program's loudness target (`audio.loudness_target`), −18 to
/// −14 LUFS.
fn profile_v10_to_v11(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the profile is not an object")?;
    let defaults = serde_json::to_value(crate::LoudnessTarget::default()).map_err(|e| e.to_string())?;
    let audio = data.entry("audio").or_insert_with(|| json!({}));
    if let Value::Object(a) = audio {
        a.entry("loudness_target").or_insert(defaults);
    }
    Ok(())
}

/// Profile version 11 to 12: the stream's automatic reconnect settings (`stream.reconnect`), on,
/// with a first wait of 2 s, a longest wait of 30 s and 10 tries.
fn profile_v11_to_v12(data: &mut Value) -> Result<(), String> {
    let data = data.as_object_mut().ok_or("the profile is not an object")?;
    let defaults = serde_json::to_value(crate::ReconnectSettings::default()).map_err(|e| e.to_string())?;
    let stream = data.entry("stream").or_insert_with(|| json!({}));
    if let Value::Object(s) = stream {
        s.entry("reconnect").or_insert(defaults);
    }
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

/// A profile file's text. A profile whose stream destinations break a rule of
/// [`crate::StreamSettings::check`], whose reconnect settings break a rule of
/// [`crate::ReconnectSettings::check`], or whose loudness target breaks a rule of
/// [`crate::LoudnessTarget::check`], is refused, as [`read_profile`] refuses its file.
pub fn write_profile(p: &Profile) -> Result<String, FormatError> {
    p.stream.check().map_err(FormatError::Data)?;
    p.stream.reconnect.check().map_err(FormatError::Data)?;
    p.audio.loudness_target.check().map_err(FormatError::Data)?;
    PROFILE.write(p)
}

/// A profile from a file's text, migrated, with video, replay buffer, audio tracks and pause
/// screen settings brought into range. A file whose stream destinations break a rule of
/// [`crate::StreamSettings::check`], whose reconnect settings break a rule of
/// [`crate::ReconnectSettings::check`], or whose loudness target breaks a rule of
/// [`crate::LoudnessTarget::check`], is refused.
pub fn read_profile(text: &str) -> Result<Profile, FormatError> {
    let mut p: Profile = PROFILE.read(text)?;
    p.stream.check().map_err(FormatError::Data)?;
    p.stream.reconnect.check().map_err(FormatError::Data)?;
    p.audio.loudness_target.check().map_err(FormatError::Data)?;
    p.video = p.video.sanitized();
    p.replay = p.replay.sanitized();
    p.audio_tracks = p.audio_tracks.sanitized();
    p.pause = p.pause.sanitized();
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
        (5, include_str!("../fixtures/collection-v5.json")),
    ];
    const PROFILE_FIXTURES: &[(u32, &str)] = &[
        (1, include_str!("../fixtures/profile-v1.json")),
        (2, include_str!("../fixtures/profile-v2.json")),
        (3, include_str!("../fixtures/profile-v3.json")),
        (4, include_str!("../fixtures/profile-v4.json")),
        (5, include_str!("../fixtures/profile-v5.json")),
        (6, include_str!("../fixtures/profile-v6.json")),
        (7, include_str!("../fixtures/profile-v7.json")),
        (8, include_str!("../fixtures/profile-v8.json")),
        (9, include_str!("../fixtures/profile-v9.json")),
        (10, include_str!("../fixtures/profile-v10.json")),
        (11, include_str!("../fixtures/profile-v11.json")),
        (12, include_str!("../fixtures/profile-v12.json")),
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
        assert_eq!(p.stream.encoder_settings.get("nvenc_h264").and_then(|s| s.str("preset")), Some("p4"), "kept under the encoder it was saved for");
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
    fn collections_keep_each_source_gain_with_its_boost() {
        use crate::collection::MAX_GAIN;
        for (v, text) in COLLECTION_FIXTURES {
            let c = read_collection(text, "Scene").unwrap();
            let gains: Vec<f32> = c.sources.iter().map(|s| s.audio.gain).collect();
            assert_eq!(gains, [1.0, 1.0, 0.5], "version {v} keeps its gains");
        }
        let (_, newest) = COLLECTION_FIXTURES[COLLECTION_FIXTURES.len() - 1];
        let gain_of = |text: &str| read_collection(text, "Scene").unwrap().source_by_name("Microphone").map(|s| s.audio.gain);
        let boosted = newest.replace("\"gain\": 0.5", "\"gain\": 4.0");
        let c = read_collection(&boosted, "Scene").unwrap();
        assert_eq!(c.source_by_name("Microphone").map(|s| s.audio.gain), Some(4.0));
        assert_eq!(read_collection(&write_collection(&c).unwrap(), "Scene").unwrap(), c, "a boosted gain saves and reloads");
        assert_eq!(gain_of(&newest.replace("\"gain\": 0.5", "\"gain\": 10.0")), Some(MAX_GAIN));
        assert_eq!(gain_of(&newest.replace("\"gain\": 0.5", "\"gain\": 1000.0")), Some(MAX_GAIN), "a gain above the top is brought down to it");
        assert_eq!(gain_of(&newest.replace("\"gain\": 0.5", "\"gain\": -2.0")), Some(0.0), "a negative gain is brought up to 0");
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
    fn collections_keep_each_item_fit_mode() {
        for (v, text) in &COLLECTION_FIXTURES[..4] {
            let c = read_collection(text, "Scene").unwrap();
            assert!(c.scenes.iter().flat_map(|s| &s.items).all(|i| i.fit.is_none()), "version {v} has no fit modes");
        }
        let (_, v5) = COLLECTION_FIXTURES[4];
        let c = read_collection(v5, "Scene").unwrap();
        let fits: Vec<_> = c.scenes[0].items.iter().map(|i| i.fit).collect();
        assert_eq!(fits, [Some(crate::FitMode::Fill), None]);
        assert_eq!(read_collection(&write_collection(&c).unwrap(), "Scene").unwrap(), c);
        let unknown = v5.replace("\"fit\": \"fill\"", "\"fit\": \"cover\"");
        assert!(matches!(read_collection(&unknown, "Scene"), Err(FormatError::Data(_))), "an unknown mode refuses the file");
    }

    #[test]
    fn profiles_keep_the_audio_tracks_settings_and_bring_sizes_into_range() {
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
            after_stop: crate::profile::AfterStop::Nothing,
            editor_path: String::new(),
        };
        assert_eq!(p.audio_tracks, want, "a version 6 profile does nothing after a take and looks the editor up on the PATH");
        assert_eq!(read_profile(&write_profile(&p).unwrap()).unwrap(), p);
        // A size out of range is brought into it, so the rest of the profile is kept.
        for (from, to, buffer, segment) in [
            ("\"buffer_mib\": 256", "\"buffer_mib\": 15", 16, 1),
            ("\"buffer_mib\": 256", "\"buffer_mib\": 1025", 1024, 1),
            ("\"segment_gib\": 1", "\"segment_gib\": 0", 256, 1),
            ("\"segment_gib\": 1", "\"segment_gib\": 4", 256, 3),
        ] {
            let p = read_profile(&v6.replace(from, to)).unwrap();
            assert_eq!((p.audio_tracks.buffer_mib, p.audio_tracks.segment_gib, p.audio_tracks.enabled), (buffer, segment, true), "{to}");
        }
        // A value of the wrong kind still refuses the file.
        for (from, to) in [("\"either\"", "\"sometimes\""), ("\"segment_gib\": 1", "\"segment_gib\": -1")] {
            let bad = v6.replace(from, to);
            assert!(matches!(read_profile(&bad), Err(FormatError::Data(_))), "{to} refuses the file");
        }
        for (from, to) in [("\"buffer_mib\": 256", "\"buffer_mib\": 16"), ("\"segment_gib\": 1", "\"segment_gib\": 3")] {
            assert!(read_profile(&v6.replace(from, to)).is_ok(), "{to} is in range");
        }
    }

    #[test]
    fn profiles_keep_the_pause_screen() {
        use crate::{FitMode, PauseAnchor, PauseAtZero, PauseSettings};
        for (v, text) in &PROFILE_FIXTURES[..9] {
            assert_eq!(read_profile(text).unwrap().pause, PauseSettings::default(), "version {v} gets the defaults");
        }
        let (_, v10) = PROFILE_FIXTURES[9];
        let p = read_profile(v10).unwrap();
        let s = &p.pause;
        assert_eq!((s.media.as_str(), s.fit, s.countdown_s, s.text.as_str()), ("C:/Media/break.mp4", FitMode::Fill, 600, "Starting again in {time}"));
        assert_eq!((s.at_zero, s.anchor, s.size_pct, s.mute_scene), (PauseAtZero::Resume, PauseAnchor::TopRight, 9, false));
        assert_eq!(read_profile(&write_profile(&p).unwrap()).unwrap(), p);
        // Values out of range are brought into range; the rest of the profile is kept.
        let wide = v10.replace("\"countdown_s\": 600", "\"countdown_s\": 99999").replace("\"size_pct\": 9", "\"size_pct\": 40");
        let p = read_profile(&wide).unwrap();
        assert_eq!((p.pause.countdown_s, p.pause.size_pct, p.replay.seconds), (43_200, 15, 45));
        let no_time = v10.replace("Starting again in {time}", "Starting again");
        assert_eq!(read_profile(&no_time).unwrap().pause.text, "Back in {time}", "a text without {{time}} reads as the default");
        assert!(matches!(read_profile(&v10.replace("\"resume\"", "\"restart\"")), Err(FormatError::Data(_))), "an unknown value refuses the file");
    }

    #[test]
    fn profiles_keep_the_loudness_target() {
        use crate::LoudnessTarget;
        for (v, text) in &PROFILE_FIXTURES[..10] {
            assert_eq!(read_profile(text).unwrap().audio.loudness_target, LoudnessTarget::default(), "version {v} gets the default");
        }
        assert_eq!(LoudnessTarget::default(), LoudnessTarget { low_lufs: -18.0, high_lufs: -14.0 });
        let (_, v11) = PROFILE_FIXTURES[10];
        let p = read_profile(v11).unwrap();
        assert_eq!(p.audio.loudness_target, LoudnessTarget { low_lufs: -24.5, high_lufs: -22.0 });
        assert_eq!(p.audio.monitoring_device, "Headphones (USB)", "the rest of the audio settings is kept");
        assert_eq!(read_profile(&write_profile(&p).unwrap()).unwrap(), p);
        let omitted = v11.replace(", \"loudness_target\": { \"low_lufs\": -24.5, \"high_lufs\": -22 }", "");
        assert_eq!(read_profile(&omitted).unwrap().audio.loudness_target, LoudnessTarget::default(), "an omitted target is the default");
        let edges = v11.replace("-24.5", "-60").replace("\"high_lufs\": -22", "\"high_lufs\": 0");
        assert_eq!(read_profile(&edges).unwrap().audio.loudness_target, LoudnessTarget { low_lufs: -60.0, high_lufs: 0.0 }, "the range's ends");
        let bad = [
            ("-24.5", "-60.5"),
            ("\"high_lufs\": -22", "\"high_lufs\": 0.5"),
            ("-24.5", "-22"),
            ("-24.5", "-20"),
            ("-24.5", "\"loud\""),
            ("-24.5", "null"),
            ("-24.5", "1e999"),
        ];
        for (from, to) in bad {
            assert!(read_profile(&v11.replace(from, to)).is_err(), "{to} refuses the file");
        }
        for target in [
            LoudnessTarget { low_lufs: -14.0, high_lufs: -18.0 },
            LoudnessTarget { low_lufs: -16.0, high_lufs: -16.0 },
            LoudnessTarget { low_lufs: f64::NAN, high_lufs: -14.0 },
            LoudnessTarget { low_lufs: -18.0, high_lufs: f64::INFINITY },
            LoudnessTarget { low_lufs: -61.0, high_lufs: -14.0 },
        ] {
            let mut wrong = p.clone();
            wrong.audio.loudness_target = target;
            assert!(matches!(write_profile(&wrong), Err(FormatError::Data(m)) if m.contains("loudness target")), "{target:?} is not written");
        }
    }

    #[test]
    fn a_v11_profile_gets_the_default_reconnect_settings() {
        use crate::ReconnectSettings;
        for (v, text) in &PROFILE_FIXTURES[..11] {
            assert_eq!(read_profile(text).unwrap().stream.reconnect, ReconnectSettings::default(), "version {v} gets the defaults");
        }
        assert_eq!(ReconnectSettings::default(), ReconnectSettings { enabled: true, first_delay_s: 2, max_delay_s: 30, max_tries: 10 });
        let (_, v12) = PROFILE_FIXTURES[11];
        let p = read_profile(v12).unwrap();
        assert_eq!(p.stream.reconnect, ReconnectSettings { enabled: true, first_delay_s: 5, max_delay_s: 20, max_tries: 4 });
        assert_eq!(p.stream.destinations.len(), 3, "the rest of the stream settings is kept");
        assert_eq!(read_profile(&write_profile(&p).unwrap()).unwrap(), p);
        let omitted = v12.replace(",\n      \"reconnect\": { \"enabled\": true, \"first_delay_s\": 5, \"max_delay_s\": 20, \"max_tries\": 4 }", "");
        let omitted =
            omitted.replace(",\r\n      \"reconnect\": { \"enabled\": true, \"first_delay_s\": 5, \"max_delay_s\": 20, \"max_tries\": 4 }", "");
        assert!(!omitted.contains("reconnect"));
        assert_eq!(read_profile(&omitted).unwrap().stream.reconnect, ReconnectSettings::default(), "omitted settings are the defaults");
        let edges = v12.replace("\"first_delay_s\": 5", "\"first_delay_s\": 60").replace("\"max_delay_s\": 20", "\"max_delay_s\": 600");
        let edges = edges.replace("\"max_tries\": 4", "\"max_tries\": 100");
        assert_eq!(
            read_profile(&edges).unwrap().stream.reconnect,
            ReconnectSettings { enabled: true, first_delay_s: 60, max_delay_s: 600, max_tries: 100 }
        );
        assert_eq!(read_profile(&v12.replace("\"first_delay_s\": 5", "\"first_delay_s\": 1")).unwrap().stream.reconnect.first_delay_s, 1);
        assert_eq!(read_profile(&v12.replace("\"max_tries\": 4", "\"max_tries\": 1")).unwrap().stream.reconnect.max_tries, 1);
    }

    #[test]
    fn out_of_range_reconnect_settings_are_refused() {
        use crate::ReconnectSettings;
        let (_, v12) = PROFILE_FIXTURES[11];
        let bad = [
            ("\"first_delay_s\": 5", "\"first_delay_s\": 0"),
            ("\"first_delay_s\": 5", "\"first_delay_s\": 61"),
            ("\"max_delay_s\": 20", "\"max_delay_s\": 4"),
            ("\"max_delay_s\": 20", "\"max_delay_s\": 601"),
            ("\"max_tries\": 4", "\"max_tries\": 0"),
            ("\"max_tries\": 4", "\"max_tries\": 101"),
            ("\"max_tries\": 4", "\"max_tries\": -1"),
            ("\"max_tries\": 4", "\"max_tries\": \"many\""),
            ("\"enabled\": true, \"first", "\"enabled\": 1, \"first"),
        ];
        for (from, to) in bad {
            let text = v12.replace(from, to);
            assert_ne!(text, v12, "{to}");
            assert!(matches!(read_profile(&text), Err(FormatError::Data(_))), "{to} refuses the file");
        }
        let p = read_profile(v12).unwrap();
        for r in [
            ReconnectSettings { first_delay_s: 0, ..p.stream.reconnect },
            ReconnectSettings { max_delay_s: 4, ..p.stream.reconnect },
            ReconnectSettings { max_tries: u32::MAX, ..p.stream.reconnect },
        ] {
            let mut wrong = p.clone();
            wrong.stream.reconnect = r;
            assert!(matches!(write_profile(&wrong), Err(FormatError::Data(m)) if m.contains("reconnect")), "{r:?} is not written");
        }
    }

    #[test]
    fn profiles_keep_what_happens_after_a_take_and_the_editor_path() {
        let (_, v7) = PROFILE_FIXTURES[6];
        let old = read_profile(v7).unwrap();
        assert_eq!(old.audio_tracks.after_stop, crate::profile::AfterStop::Nothing, "a version 7 profile does nothing after a take");
        assert_eq!(old.audio_tracks.editor_path, "", "a version 7 profile looks the editor up on the PATH");
        let (_, v8) = PROFILE_FIXTURES[7];
        let p = read_profile(v8).unwrap();
        assert_eq!(p.audio_tracks.after_stop, crate::profile::AfterStop::OpenEditor);
        assert_eq!(p.audio_tracks.editor_path, "C:/Tools/Editor/editor.exe");
        assert_eq!(read_profile(&write_profile(&p).unwrap()).unwrap(), p);
        for (id, want) in [("nothing", crate::profile::AfterStop::Nothing), ("show_folder", crate::profile::AfterStop::ShowFolder)] {
            let p = read_profile(&v8.replace("\"open_editor\"", &format!("\"{id}\""))).unwrap();
            assert_eq!(p.audio_tracks.after_stop, want, "{id}");
        }
        assert!(matches!(read_profile(&v8.replace("\"open_editor\"", "\"launch\"")), Err(FormatError::Data(_))), "an unknown value refuses the file");
        // A path too long to be one, or holding a NUL, is cleared so the rest of the profile is kept.
        let long = "x".repeat(crate::profile::AudioTracksSettings::MAX_EDITOR_PATH + 1);
        for bad in [long.as_str(), "C:/a\\u0000b.exe"] {
            let p = read_profile(&v8.replace("C:/Tools/Editor/editor.exe", bad)).unwrap();
            assert_eq!((p.audio_tracks.editor_path.as_str(), p.audio_tracks.enabled), ("", true), "{bad}");
        }
    }

    #[test]
    fn a_profile_with_one_server_streams_to_one_destination_with_the_profile_key() {
        use crate::profile::{Destination, KeyRef, PRIMARY_DESTINATION};
        for (v, text) in &PROFILE_FIXTURES[..6] {
            let p = read_profile(text).unwrap();
            let want = Destination {
                id: PRIMARY_DESTINATION.into(),
                name: String::new(),
                enabled: true,
                server: "rtmp://localhost/live".into(),
                key: KeyRef::Profile,
                service: String::new(),
            };
            assert_eq!(p.stream.destinations, vec![want], "version {v}");
            assert_eq!(p.stream.key, "test", "version {v} keeps its key");
            // Version 1 streams with x264, the later fixtures with NVENC.
            let built_in = if *v == 1 { "h264" } else { "nvenc_native_h264" };
            assert_eq!(p.stream.video_encoder, built_in, "version {v}: the built-in form of the encoder");
        }
        let (_, v6) = PROFILE_FIXTURES[5];
        let no_server = v6.replace("\"server\": \"rtmp://localhost/live\", ", "");
        assert!(read_profile(&no_server).unwrap().stream.destinations.is_empty(), "no server, no destination");
        let blank = v6.replace("\"rtmp://localhost/live\"", "\"  \"");
        assert!(read_profile(&blank).unwrap().stream.destinations.is_empty(), "a blank server is no destination");
        let not_text = v6.replace("\"rtmp://localhost/live\"", "5");
        assert!(matches!(read_profile(&not_text), Err(FormatError::Migration { from: 6, .. })), "a server that is not text refuses the file");
        let changed = [
            ("x264", "h264"),
            ("mf_h264", "mf_native_h264"),
            ("vt_h264", "vt_native_h264"),
            ("vaapi_h264", "vaapi_native_h264"),
            ("amf_h264", "hardware"),
            ("qsv_h264", "hardware"),
        ];
        for (from, to) in changed {
            let text = v6.replace("\"video_encoder\": \"nvenc_h264\"", &format!("\"video_encoder\": \"{from}\""));
            assert_eq!(read_profile(&text).unwrap().stream.video_encoder, to);
        }
        for kept in ["hardware", "h264", "nvenc_native_h264"] {
            let text = v6.replace("\"video_encoder\": \"nvenc_h264\"", &format!("\"video_encoder\": \"{kept}\""));
            assert_eq!(read_profile(&text).unwrap().stream.video_encoder, kept);
        }
        assert_eq!(
            read_profile(&v6.replace("\"video_encoder\": \"hardware\"", "\"video_encoder\": \"x264\"")).unwrap().output.video_encoder,
            "x264",
            "recordings keep ffmpeg's x264"
        );
    }

    #[test]
    fn profiles_keep_each_destination_and_refuse_bad_lists() {
        use crate::profile::{KeyRef, MAX_DESTINATIONS};
        let (_, v7) = PROFILE_FIXTURES[6];
        let p = read_profile(v7).unwrap();
        let keys: Vec<(&str, bool, &KeyRef)> = p.stream.destinations.iter().map(|d| (d.id.as_str(), d.enabled, &d.key)).collect();
        assert_eq!(keys, [("main", true, &KeyRef::Profile), ("backup", false, &KeyRef::Held("backup".into())), ("open", true, &KeyRef::None)]);
        assert_eq!(p.stream.destination("backup").map(|d| d.server.as_str()), Some("rtmps://backup.localhost/app"));
        let text = write_profile(&p).unwrap();
        assert_eq!(read_profile(&text).unwrap(), p);
        assert_eq!(text.matches("\"test\"").count(), 1, "only the profile's own key is in the file");
        let twice = v7.replace("\"id\": \"backup\"", "\"id\": \"main\"");
        assert!(matches!(read_profile(&twice), Err(FormatError::Data(m)) if m.contains("used twice")));
        let empty = v7.replace("\"id\": \"backup\"", "\"id\": \" \"");
        assert!(matches!(read_profile(&empty), Err(FormatError::Data(m)) if m.contains("no id")));
        let long = v7.replace("\"id\": \"backup\"", &format!("\"id\": \"{}\"", "b".repeat(65)));
        assert!(matches!(read_profile(&long), Err(FormatError::Data(m)) if m.contains("longer than 64")));
        let unknown = v7.replace("\"key\": \"none\"", "\"key\": \"vault\"");
        assert!(matches!(read_profile(&unknown), Err(FormatError::Data(_))), "an unknown key place refuses the file");
        let mut many = p.clone();
        many.stream.destinations = (0..=MAX_DESTINATIONS).map(|i| crate::Destination { id: format!("d{i}"), ..Default::default() }).collect();
        assert!(matches!(write_profile(&many), Err(FormatError::Data(m)) if m.contains("at most 16")), "a profile that could not be read back");
        many.stream.destinations.pop();
        let sixteen = write_profile(&many).unwrap();
        assert!(read_profile(&sixteen).is_ok(), "16 destinations are allowed");
        let mut file: Value = serde_json::from_str(&sixteen).unwrap();
        let Some(Value::Array(list)) = file.pointer_mut("/data/stream/destinations") else { panic!("no destinations in {sixteen}") };
        list.push(json!({"id": "d16"}));
        assert!(matches!(read_profile(&file.to_string()), Err(FormatError::Data(m)) if m.contains("at most 16")));
        let mut twice = p.clone();
        twice.stream.destinations[1].id = "main".into();
        assert!(matches!(write_profile(&twice), Err(FormatError::Data(m)) if m.contains("used twice")));
    }

    #[test]
    fn profiles_keep_the_service_each_destination_was_set_up_for() {
        for (v, text) in &PROFILE_FIXTURES[..8] {
            let p = read_profile(text).unwrap();
            assert!(p.stream.destinations.iter().all(|d| d.service.is_empty()), "version {v}: every destination is a custom server");
        }
        let (_, v9) = PROFILE_FIXTURES[8];
        let p = read_profile(v9).unwrap();
        let services: Vec<(&str, &str)> = p.stream.destinations.iter().map(|d| (d.id.as_str(), d.service.as_str())).collect();
        assert_eq!(services, [("main", "twitch"), ("backup", ""), ("open", "retired-service")]);
        assert_eq!(crate::services::service(&p.stream.destinations[0].service).map(|s| s.id), Some("twitch"));
        assert_eq!(crate::services::service(&p.stream.destinations[2].service), None, "an unknown service is kept and has no preset");
        assert_eq!(read_profile(&write_profile(&p).unwrap()).unwrap(), p);
        let wrong = v9.replace("\"service\": \"twitch\"", "\"service\": 3");
        assert!(matches!(read_profile(&wrong), Err(FormatError::Data(_))), "a service that is not text refuses the file");
        // A destination of an older file that already names a service keeps it.
        let (_, v8) = PROFILE_FIXTURES[7];
        let named = v8.replace("\"key\": \"profile\" }", "\"key\": \"profile\", \"service\": \"youtube\" }");
        assert_eq!(read_profile(&named).unwrap().stream.destinations[0].service, "youtube");
    }

    #[test]
    fn the_primary_destination_is_made_with_the_profile_key() {
        let mut s = crate::StreamSettings::default();
        assert_eq!(s.primary_server(), "");
        s.primary_mut().server = "rtmp://a/live".into();
        assert_eq!(s.destinations.len(), 1);
        assert_eq!((s.destinations[0].id.as_str(), &s.destinations[0].key), ("main", &crate::KeyRef::Profile));
        s.destinations.push(crate::Destination { id: "b".into(), server: "rtmp://b/live".into(), ..Default::default() });
        s.primary_mut().enabled = false;
        assert!(!s.destinations[0].enabled && s.destinations[1].enabled, "the first destination is the one edited");
        assert_eq!(s.primary_server(), "rtmp://a/live");
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
