//! What the compositor draws: a description the interface thread publishes whenever the scene
//! collection, the program scene or a transition changes. The compositor keeps the newest one
//! and draws it every frame.

use std::sync::Arc;

use studiocraft_media::Tick;
use studiocraft_model::{SourceId, Transform};
use studiocraft_plugin::{VideoEffect, VideoOut};

/// One visible item of a scene.
#[derive(Clone)]
pub struct ItemDesc {
    pub source: SourceId,
    pub video: Arc<VideoOut>,
    pub transform: Transform,
    /// The source's enabled video filters, in order.
    pub effects: Vec<VideoEffect>,
}

/// A scene's visible items, bottom to top.
#[derive(Clone, Default)]
pub struct SceneDesc {
    pub items: Vec<ItemDesc>,
}

/// A transition running in the program from `from` to the plan's program scene.
#[derive(Clone)]
pub struct TransitionDesc {
    pub from: SceneDesc,
    /// Identifies the shader: one pipeline is compiled per key.
    pub key: String,
    /// WGSL defining `fn transition(...)` (see `studiocraft_plugin::Transition`).
    pub wgsl: &'static str,
    pub params: [f32; 4],
    /// When the transition started and how long it runs, on the master clock.
    pub start: Tick,
    pub duration: Tick,
}

impl TransitionDesc {
    /// Progress at `now`, 0 to 1.
    pub fn progress(&self, now: Tick) -> f32 {
        if self.duration.0 <= 0 {
            return 1.0;
        }
        let t = now.0.saturating_sub(self.start.0) as f64 / self.duration.0 as f64;
        t.clamp(0.0, 1.0) as f32
    }
}

/// Everything drawn in a frame.
#[derive(Clone, Default)]
pub struct Plan {
    pub program: SceneDesc,
    pub transition: Option<TransitionDesc>,
    /// The preview scene in studio mode.
    pub preview: Option<SceneDesc>,
}

impl Plan {
    /// Every item in the plan.
    pub fn items(&self) -> impl Iterator<Item = &ItemDesc> {
        let from = self.transition.iter().flat_map(|t| t.from.items.iter());
        let preview = self.preview.iter().flat_map(|p| p.items.iter());
        self.program.items.iter().chain(from).chain(preview)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transition_progress_is_clamped() {
        let t = TransitionDesc { from: SceneDesc::default(), key: "fade".into(), wgsl: "", params: [0.0; 4], start: Tick(100), duration: Tick(200) };
        assert_eq!(t.progress(Tick(0)), 0.0);
        assert_eq!(t.progress(Tick(200)), 0.5);
        assert_eq!(t.progress(Tick(900)), 1.0);
        let instant = TransitionDesc { duration: Tick(0), ..t };
        assert_eq!(instant.progress(Tick(0)), 1.0);
    }
}
