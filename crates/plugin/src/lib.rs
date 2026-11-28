//! StudioCraft's extension points.
//!
//! Every source, filter, transition, encoder and output is a type implementing one of the traits
//! here, registered in the [`Registry`] at startup. Each declares an id (stored in files, stable
//! across versions), an English name, a version, and a [`SettingsSchema`] from which the
//! properties window is built. Adding one is adding a type and one `register_*` call: nothing
//! else in the app changes.
//!
//! | Trait | Runs on | Produces |
//! |---|---|---|
//! | [`Source`] | its own thread ([`SourceInstance`]) | video frames into a mailbox, audio blocks into a queue |
//! | [`Filter`] | the compositor (video) or mixer (audio) thread | a GPU [`VideoEffect`] pass, or processed audio |
//! | [`Transition`] | the compositor thread | a GPU blend between two scenes |
//! | [`Encoder`] | an output's encoder thread, or inside its ffmpeg process | packets ([`VideoSession`]), or codec arguments for a rate control |
//! | [`Output`] | its own thread ([`OutputInstance`]) | a recording file, a stream, or the replay buffer's clips |
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod encoder;
mod filter;
mod output;
mod registry;
mod source;
mod transition;

pub use craft_ease::Easing;
pub use encoder::{Availability, Codec, Encoder, EncoderEnv, EncoderKind, FfmpegEncoder, Packet, RateControl, VideoParams, VideoSession};
pub use filter::{
    Animation, AnimationStep, EffectImage, Filter, FilterInstance, FilterMeter, FilterTarget, MAX_EFFECT_SIDE, Reframe, VIDEO_EFFECT_PARAMS,
    VideoEffect,
};
pub use output::{
    AddressOutcome, Cause, DestinationCounts, DestinationSetup, DestinationState, DestinationStatus, Failure, Output, OutputInstance, OutputJob,
    OutputStatus, OwnEncoder, ReplayClip, StreamCopyState, VideoFormat,
};
pub use registry::Registry;
pub use source::{AUDIO_QUEUE_BLOCKS, AudioOut, Pace, Source, SourceCaps, SourceInstance, SourceIo, VideoContent, VideoOut};
pub use studiocraft_model::{Settings, SettingsSchema};
pub use transition::{Transition, easing_field, easing_from};

use thiserror::Error;

/// Why a plug-in could not do what was asked.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum PluginError {
    #[error("no {kind} plug-in has the id `{id}`")]
    Unknown { kind: &'static str, id: String },
    #[error("a {kind} plug-in with the id `{id}` is already registered")]
    Duplicate { kind: &'static str, id: String },
    #[error("{0}")]
    Unavailable(String),
    #[error("{0}")]
    Failed(String),
}

impl PluginError {
    pub fn failed(msg: impl std::fmt::Display) -> Self {
        PluginError::Failed(msg.to_string())
    }
}

/// What every plug-in declares.
pub trait Plugin: Send + Sync {
    /// Stable id, stored in saved files.
    fn id(&self) -> &'static str;
    /// English name; the interface translates it.
    fn name(&self) -> &'static str;
    /// Version of the plug-in's settings; files record it, and [`Plugin::migrate`] upgrades
    /// settings written by an older version.
    fn version(&self) -> u32 {
        1
    }
    /// The settings and how the properties window shows them.
    fn schema(&self) -> SettingsSchema;
    /// The schema for a plug-in set up as `settings`, for lists that depend on another setting
    /// (the formats of the chosen camera). New settings are checked against it.
    fn schema_for(&self, settings: &Settings) -> SettingsSchema {
        let _ = settings;
        self.schema()
    }
    /// A value that changes when the devices the schema lists change (a camera plugged in or
    /// out), quick enough to read every few seconds. `None` when the lists do not change on
    /// their own.
    fn lists_stamp(&self) -> Option<u64> {
        None
    }
    /// Settings written by version `from` brought up to [`Plugin::version`].
    fn migrate(&self, from: u32, settings: Settings) -> Settings {
        let _ = from;
        settings
    }
}
