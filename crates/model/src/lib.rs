//! StudioCraft's data model: scene collections (scenes, sources, scene items, transforms, filters,
//! the transition), profiles (video, audio, output and stream settings), the streaming services a
//! stream destination can be set up for, the settings schemas plug-ins declare, and the slots of
//! the Staged windows source ([`staged`]). Pure data with serde; no I/O. Saved files carry a format version and are
//! migrated step by step on load ([`format`]).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod collection;
pub mod format;
pub mod geom;
pub mod ids;
pub mod profile;
pub mod schema;
pub mod services;
pub mod settings;
pub mod staged;

pub use collection::{AudioState, FilterConfig, MonitorMode, Scene, SceneCollection, SceneItem, SourceConfig, TransitionConfig};
pub use format::FormatError;
pub use geom::{Crop, FitMode, Transform, Vec2};
pub use ids::{FilterId, ItemId, SceneId, SourceId};
pub use profile::{
    AfterStop, AudioSettings, AudioTracksSettings, AudioTracksWhen, Destination, KeyRef, OUTPUT_SCALES, OutputSettings, PauseAnchor, PauseAtZero,
    PauseSettings, Profile, RecordingFormat, RecordingQuality, ReplaySettings, StreamSettings, VideoSettings,
};
pub use schema::{ChoiceOption, Field, FieldKind, PathMode, SettingsSchema};
pub use services::{SERVICES, Service};
pub use settings::Settings;
