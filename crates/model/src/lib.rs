//! StudioCraft's data model: scene collections (scenes, sources, scene items, transforms, filters,
//! the transition), profiles (video, audio, output and stream settings) and the settings schemas
//! plug-ins declare. Pure data with serde; no I/O. Saved files carry a format version and are
//! migrated step by step on load ([`format`]).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod collection;
pub mod format;
pub mod geom;
pub mod ids;
pub mod profile;
pub mod schema;
pub mod settings;

pub use collection::{AudioState, FilterConfig, MonitorMode, Scene, SceneCollection, SceneItem, SourceConfig, TransitionConfig};
pub use format::FormatError;
pub use geom::{Crop, Transform, Vec2};
pub use ids::{FilterId, ItemId, SceneId, SourceId};
pub use profile::{
    AudioSettings, AudioTracksSettings, AudioTracksWhen, Destination, KeyRef, OutputSettings, Profile, RecordingFormat, RecordingQuality,
    ReplaySettings, StreamSettings, VideoSettings,
};
pub use schema::{ChoiceOption, Field, FieldKind, PathMode, SettingsSchema};
pub use settings::Settings;
