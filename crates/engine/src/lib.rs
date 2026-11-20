//! StudioCraft's engine.
//!
//! - [`command`]: every user action as a typed [`Command`] with a stable id. The interface, the
//!   hotkeys, scripts and the control channel all run actions through [`Engine::execute`]; the
//!   interface never changes the model itself.
//! - [`credits`]: the contributors listed in the About window.
//! - [`history`]: undo and redo of model changes.
//! - [`host`]: one thread per source, created from the registry.
//! - [`lanes`]: the lanes of the Application Audio panel.
//! - [`links`]: the community and project links and the app's identity.
//! - [`mixer`]: the audio mixer thread.
//! - [`scheduler`]: the thread that wakes the compositor once per frame on the master clock.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod command;
pub mod credits;
#[cfg(test)]
mod credits_gen;
mod engine;
pub mod history;
pub mod host;
pub mod lanes;
pub mod links;
pub mod mixer;
pub mod scheduler;

pub use command::{Command, CommandSpec, ItemRef, Move, RuntimeAction, SceneRef, SourceRef, specs};
pub use engine::{Engine, EngineError, Outcome};
