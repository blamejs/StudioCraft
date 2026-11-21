//! What the compositor draws: a description the interface thread publishes whenever the scene
//! collection, the program scene or a transition changes. The compositor keeps the newest one
//! and draws it every frame.

use std::sync::Arc;
use std::time::Duration;

use studiocraft_media::Tick;
use studiocraft_media::clock::tick_of;
use studiocraft_model::{SourceId, Transform};
use studiocraft_plugin::{Easing, VideoEffect, VideoOut};

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
    /// How time through the duration becomes the shader's progress.
    pub ease: Easing,
}

impl TransitionDesc {
    /// Progress at `now` after the ease: 0 at the start, 1 at the end and after it.
    pub fn progress(&self, now: Tick) -> f32 {
        if self.duration.0 <= 0 {
            return 1.0;
        }
        let t = now.0.saturating_sub(self.start.0) as f64 / self.duration.0 as f64;
        self.ease.apply(t.clamp(0.0, 1.0)) as f32
    }

    /// Whether the transition is still drawn at `now`: until its duration has passed, whatever
    /// value the ease gives on the way.
    pub fn running(&self, now: Tick) -> bool {
        now.0.saturating_sub(self.start.0) < self.duration.0
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

/// Most scenes one [`ThumbnailRequest`] draws; the rest are skipped and counted.
pub const MAX_THUMBNAILS: usize = 16;
/// Most pictures per second a [`ThumbnailRequest`] draws.
pub const MAX_THUMBNAIL_FPS: f32 = 4.0;
/// Smallest side of a thumbnail's box, in pixels.
pub const MIN_THUMBNAIL_SIDE: u32 = 8;
/// Largest side of a thumbnail's box, in pixels.
pub const MAX_THUMBNAIL_SIDE: u32 = 512;

/// Scenes to draw as small pictures, for scene lists. The compositor draws them beside the
/// program into textures of their own (see `ThumbnailViews`) and reads nothing back.
#[derive(Clone)]
pub struct ThumbnailRequest {
    /// The scenes, in slot order: scene `i` is drawn into slot `i`. Only the first
    /// [`MAX_THUMBNAILS`] are drawn.
    pub scenes: Vec<SceneDesc>,
    /// Pictures per second, at most [`MAX_THUMBNAIL_FPS`]. Zero, a negative value or NaN draws
    /// each scene once per request.
    pub fps: f32,
    /// The box each picture fits in, in pixels. The picture keeps the canvas's aspect ratio.
    pub size: (u32, u32),
    /// What each scene is, in slot order, such as its scene id: a slot whose key changes is
    /// drawn again soon, as one that shows other sources is. Slots past the end have no key.
    pub keys: Vec<u64>,
}

impl Default for ThumbnailRequest {
    fn default() -> Self {
        Self { scenes: Vec::new(), fps: 2.0, size: (256, 144), keys: Vec::new() }
    }
}

impl ThumbnailRequest {
    /// The scenes that are drawn.
    pub fn drawn(&self) -> &[SceneDesc] {
        &self.scenes[..self.scenes.len().min(MAX_THUMBNAILS)]
    }

    /// How many scenes are past [`MAX_THUMBNAILS`] and not drawn.
    pub fn skipped(&self) -> usize {
        self.scenes.len().saturating_sub(MAX_THUMBNAILS)
    }

    /// The shortest time between two pictures of a scene, or `None` to draw once per request. A
    /// rate so low that its interval is longer than a [`Duration`] holds also draws once.
    pub fn interval(&self) -> Option<Tick> {
        if self.fps.is_nan() || self.fps <= 0.0 {
            return None;
        }
        let fps = self.fps.min(MAX_THUMBNAIL_FPS);
        Duration::try_from_secs_f64(1.0 / f64::from(fps)).ok().map(tick_of)
    }

    /// The picture size for a `canvas`-sized canvas: as large as fits in [`ThumbnailRequest::size`]
    /// (each side clamped to [`MIN_THUMBNAIL_SIDE`]..=[`MAX_THUMBNAIL_SIDE`] and to `limit`) at
    /// the canvas's aspect ratio, and at least 1 x 1.
    pub fn picture_size(&self, (cw, ch): (u32, u32), limit: u32) -> (u32, u32) {
        let max = MAX_THUMBNAIL_SIDE.min(limit.max(1));
        let side = |v: u32| v.clamp(MIN_THUMBNAIL_SIDE.min(max), max);
        let (bw, bh) = (side(self.size.0), side(self.size.1));
        let (cw, ch) = (f64::from(cw.max(1)), f64::from(ch.max(1)));
        let scale = (f64::from(bw) / cw).min(f64::from(bh) / ch);
        let fit = |v: f64, bound: u32| ((v * scale).round() as u32).clamp(1, bound);
        (fit(cw, bw), fit(ch, bh))
    }

    /// Every item of the drawn scenes.
    pub fn items(&self) -> impl Iterator<Item = &ItemDesc> {
        self.drawn().iter().flat_map(|s| s.items.iter())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_thumbnail_keeps_the_canvas_aspect_inside_its_box() {
        let r = ThumbnailRequest::default();
        assert_eq!(r.picture_size((1920, 1080), 8192), (256, 144));
        assert_eq!(r.picture_size((1080, 1920), 8192), (81, 144));
        assert_eq!(r.picture_size((1000, 1000), 8192), (144, 144));
        // A very wide canvas still gives at least one row.
        assert_eq!(r.picture_size((8192, 16), 8192), (256, 1));
        // A box outside the limits is clamped to them, and to the device's.
        let big = ThumbnailRequest { size: (100_000, 100_000), ..ThumbnailRequest::default() };
        assert_eq!(big.picture_size((1920, 1080), 8192), (512, 288));
        assert_eq!(big.picture_size((1920, 1080), 64), (64, 36));
        let tiny = ThumbnailRequest { size: (0, 0), ..ThumbnailRequest::default() };
        assert_eq!(tiny.picture_size((1600, 800), 8192), (8, 4));
        // A zero canvas is treated as 1 x 1.
        assert_eq!(r.picture_size((0, 0), 8192), (144, 144));
    }

    #[test]
    fn a_thumbnail_request_draws_sixteen_scenes_and_counts_the_rest() {
        let mut r = ThumbnailRequest::default();
        assert_eq!((r.drawn().len(), r.skipped()), (0, 0));
        r.scenes = vec![SceneDesc::default(); MAX_THUMBNAILS];
        assert_eq!((r.drawn().len(), r.skipped()), (MAX_THUMBNAILS, 0));
        r.scenes.push(SceneDesc::default());
        r.scenes.push(SceneDesc::default());
        assert_eq!((r.drawn().len(), r.skipped()), (MAX_THUMBNAILS, 2));
    }

    #[test]
    fn the_thumbnail_rate_is_capped() {
        let at = |fps| ThumbnailRequest { fps, ..ThumbnailRequest::default() }.interval();
        assert_eq!(at(2.0), Some(tick_of(Duration::from_millis(500))));
        assert_eq!(at(4.0), Some(tick_of(Duration::from_millis(250))));
        assert_eq!(at(60.0), Some(tick_of(Duration::from_millis(250))));
        assert_eq!(at(f32::INFINITY), Some(tick_of(Duration::from_millis(250))));
        assert_eq!(at(0.0), None);
        assert_eq!(at(-1.0), None);
        assert_eq!(at(f32::NAN), None);
        // A rate whose interval no duration holds draws once instead of failing.
        assert_eq!(at(1e-30), None);
        assert_eq!(at(f32::MIN_POSITIVE), None);
        assert!(at(1e-6).is_some_and(|t| t.0 > 0));
        // Long intervals stay exact.
        assert_eq!(at(0.0625), Some(Tick::from_seconds_f64(16.0)));
        assert_eq!(at(1.0 / 1024.0), Some(Tick::from_seconds_f64(1024.0)));
        assert!(at(1.0 / 1024.0).is_some_and(|t| (t.seconds() - 1024.0).abs() < 1e-6));
    }

    #[test]
    fn transition_progress_is_clamped() {
        let t = TransitionDesc {
            from: SceneDesc::default(),
            key: "fade".into(),
            wgsl: "",
            params: [0.0; 4],
            start: Tick(100),
            duration: Tick(200),
            ease: Easing::Linear,
        };
        assert_eq!(t.progress(Tick(0)), 0.0);
        assert_eq!(t.progress(Tick(200)), 0.5);
        assert_eq!(t.progress(Tick(900)), 1.0);
        let instant = TransitionDesc { duration: Tick(0), ..t };
        assert_eq!(instant.progress(Tick(0)), 1.0);
    }

    #[test]
    fn transition_progress_follows_the_ease() {
        let t = TransitionDesc {
            from: SceneDesc::default(),
            key: "fade".into(),
            wgsl: "",
            params: [0.0; 4],
            start: Tick(100),
            duration: Tick(200),
            ease: Easing::QuadIn,
        };
        assert_eq!(t.progress(Tick(200)), 0.25);
        assert_eq!(t.progress(Tick(100)), 0.0);
        assert_eq!(t.progress(Tick(300)), 1.0);
        assert_eq!(t.progress(Tick(1_000_000)), 1.0);
        let out = TransitionDesc { ease: Easing::QuadOut, ..t };
        assert_eq!(out.progress(Tick(200)), 0.75);
    }

    #[test]
    fn a_transition_runs_for_its_duration_whatever_its_ease_gives() {
        let t = TransitionDesc {
            from: SceneDesc::default(),
            key: "fade".into(),
            wgsl: "",
            params: [0.0; 4],
            start: Tick(100),
            duration: Tick(200),
            ease: Easing::BackOut,
        };
        // Back Out passes 1 before the end and comes back to it.
        assert!(t.progress(Tick(250)) >= 1.0, "{}", t.progress(Tick(250)));
        assert!(t.running(Tick(250)) && t.running(Tick(100)) && t.running(Tick(299)));
        assert!(!t.running(Tick(300)) && !t.running(Tick(1_000_000)));
        assert!(!TransitionDesc { duration: Tick(0), ..t }.running(Tick(100)));
    }
}
