//! Media plumbing shared by every thread of the pipeline:
//!
//! - [`clock`]: the master clock all timestamps come from, and frame pacing on it.
//! - [`video`] and [`audio`]: frame and audio block types, pixel formats and sample layout.
//! - [`pool`]: buffers reused from frame to frame, so the hot path does not allocate.
//! - [`queue`]: bounded lock-free queues between threads.
//! - [`mix`]: mixing, gain and level metering.
//! - [`stats`]: counters the stats window reads without locking.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

pub mod audio;
pub mod clock;
pub mod mix;
pub mod pool;
pub mod queue;
pub mod stats;
pub mod video;

pub use craft_time::{FrameRate, Tick};
