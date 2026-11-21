//! Placement of a scene item on the canvas.
//!
//! A transform stores a scale rather than a size: a source's own size can change while it runs
//! (a captured window is resized, a text is edited), and the item keeps its proportions. The
//! size on the canvas is the cropped source size times the scale ([`Transform::size`]).

use serde::{Deserialize, Serialize};

/// A point, size or scale in canvas pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Vec2 {
    pub x: f32,
    pub y: f32,
}

impl Vec2 {
    pub const ZERO: Vec2 = Vec2 { x: 0.0, y: 0.0 };
    pub const ONE: Vec2 = Vec2 { x: 1.0, y: 1.0 };

    pub const fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }

    /// Both components finite.
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite()
    }
}

/// Pixels cut from each edge of the source before it is placed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Crop {
    pub left: u32,
    pub top: u32,
    pub right: u32,
    pub bottom: u32,
}

impl Crop {
    /// The size left of a `width` x `height` source after cropping, at least 1 x 1.
    pub fn apply(self, width: u32, height: u32) -> (u32, u32) {
        let w = width.saturating_sub(self.left.saturating_add(self.right)).max(1);
        let h = height.saturating_sub(self.top.saturating_add(self.bottom)).max(1);
        (w, h)
    }
}

/// Largest position accepted, in canvas pixels (16 times an 8K canvas).
pub const MAX_EXTENT: f32 = 16.0 * 7680.0;
/// Smallest and largest scale accepted.
pub const MIN_SCALE: f32 = 0.001;
pub const MAX_SCALE: f32 = 1000.0;

/// Where and how a scene item is drawn: the top-left corner of its unrotated box, its scale,
/// its clockwise rotation in degrees around the box's center, and the crop applied to the
/// source first.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Transform {
    pub position: Vec2,
    pub scale: Vec2,
    pub rotation: f32,
    pub crop: Crop,
}

impl Default for Transform {
    fn default() -> Self {
        Self { position: Vec2::ZERO, scale: Vec2::ONE, rotation: 0.0, crop: Crop::default() }
    }
}

impl Transform {
    /// An error when a value is not finite or out of range: scales between [`MIN_SCALE`] and
    /// [`MAX_SCALE`], positions within plus or minus [`MAX_EXTENT`]. Any finite rotation is
    /// accepted.
    pub fn check(&self) -> Result<(), String> {
        let (p, s) = (self.position, self.scale);
        if ![p.x, p.y].iter().all(|v| v.is_finite() && v.abs() <= MAX_EXTENT) {
            return Err(format!("`position` must be within {MAX_EXTENT} pixels of the canvas origin"));
        }
        if ![s.x, s.y].iter().all(|v| v.is_finite() && (MIN_SCALE..=MAX_SCALE).contains(v)) {
            return Err(format!("`scale` must be from {MIN_SCALE} to {MAX_SCALE}"));
        }
        if !self.rotation.is_finite() {
            return Err("`rotation` must be a finite number of degrees".into());
        }
        Ok(())
    }

    /// A transform with finite values in range: scales between [`MIN_SCALE`] and [`MAX_SCALE`],
    /// positions within plus or minus [`MAX_EXTENT`], rotation in `[0, 360)`.
    pub fn sanitized(self) -> Transform {
        let clamp_pos = |v: f32| if v.is_finite() { v.clamp(-MAX_EXTENT, MAX_EXTENT) } else { 0.0 };
        let clamp_scale = |v: f32| if v.is_finite() { v.clamp(MIN_SCALE, MAX_SCALE) } else { 1.0 };
        let rotation = if self.rotation.is_finite() { self.rotation.rem_euclid(360.0) } else { 0.0 };
        Transform {
            position: Vec2::new(clamp_pos(self.position.x), clamp_pos(self.position.y)),
            scale: Vec2::new(clamp_scale(self.scale.x), clamp_scale(self.scale.y)),
            rotation,
            crop: self.crop,
        }
    }

    /// The item's size on the canvas for a source of `native` size.
    pub fn size(&self, native: (u32, u32)) -> Vec2 {
        let (w, h) = self.crop.apply(native.0, native.1);
        Vec2::new(w as f32 * self.scale.x, h as f32 * self.scale.y)
    }

    /// The center of the item on the canvas.
    pub fn center(&self, native: (u32, u32)) -> Vec2 {
        let s = self.size(native);
        Vec2::new(self.position.x + s.x * 0.5, self.position.y + s.y * 0.5)
    }

    /// The four corners on the canvas after rotation, clockwise from the top left.
    pub fn corners(&self, native: (u32, u32)) -> [Vec2; 4] {
        let size = self.size(native);
        let c = self.center(native);
        let (s, co) = self.rotation.to_radians().sin_cos();
        let (hx, hy) = (size.x * 0.5, size.y * 0.5);
        [(-hx, -hy), (hx, -hy), (hx, hy), (-hx, hy)].map(|(x, y)| Vec2::new(c.x + x * co - y * s, c.y + x * s + y * co))
    }

    /// `p` in the item's own unrotated box, with (0, 0) at its top-left corner.
    pub fn to_local(&self, native: (u32, u32), p: Vec2) -> Vec2 {
        let size = self.size(native);
        let c = self.center(native);
        let (s, co) = (-self.rotation).to_radians().sin_cos();
        let (dx, dy) = (p.x - c.x, p.y - c.y);
        Vec2::new(dx * co - dy * s + size.x * 0.5, dx * s + dy * co + size.y * 0.5)
    }

    /// Whether the canvas point `p` lies inside the rotated item.
    pub fn contains(&self, native: (u32, u32), p: Vec2) -> bool {
        let size = self.size(native);
        let l = self.to_local(native, p);
        l.x >= 0.0 && l.y >= 0.0 && l.x <= size.x && l.y <= size.y
    }

    /// The same transform with its scale set so the item is `size` on the canvas.
    pub fn with_size(self, native: (u32, u32), size: Vec2) -> Transform {
        let (w, h) = self.crop.apply(native.0, native.1);
        Transform { scale: Vec2::new(size.x / w as f32, size.y / h as f32), ..self }.sanitized()
    }

    /// A transform that scales and centers a `native` source to fit a `canvas`, keeping its
    /// aspect ratio. A source that needs a scale past [`MIN_SCALE`] or [`MAX_SCALE`] is drawn
    /// at that limit, still centered.
    pub fn fit(native: (u32, u32), canvas: (u32, u32)) -> Transform {
        let (w, h) = (native.0.max(1) as f32, native.1.max(1) as f32);
        let (cw, ch) = (canvas.0.max(1) as f32, canvas.1.max(1) as f32);
        let k = (cw / w).min(ch / h).clamp(MIN_SCALE, MAX_SCALE);
        Transform { position: Vec2::new((cw - w * k) * 0.5, (ch - h * k) * 0.5), scale: Vec2::new(k, k), ..Transform::default() }
    }

    /// A transform that scales and centers a `native` source to cover a `canvas`, keeping its
    /// aspect ratio. The parts past the canvas's edges are not drawn. A source that needs a scale
    /// past [`MIN_SCALE`] or [`MAX_SCALE`] is drawn at that limit, still centered, and then does
    /// not cover the canvas (a 1 x 1 source on a canvas wider than 1000 pixels). A source so
    /// long and thin that covering the canvas would put its edges past [`MAX_EXTENT`] is drawn
    /// smaller, so its edges stay within it and it stays centered (65536 x 1 on 8192 x 8192).
    pub fn fill(native: (u32, u32), canvas: (u32, u32)) -> Transform {
        let (w, h) = (native.0.max(1) as f32, native.1.max(1) as f32);
        let (cw, ch) = (canvas.0.max(1) as f32, canvas.1.max(1) as f32);
        // One pixel short of the limit, so rounding does not take an edge past it.
        let reach = 2.0 * (MAX_EXTENT - 1.0);
        let within = ((cw + reach) / w).min((ch + reach) / h);
        let k = (cw / w).max(ch / h).min(within).clamp(MIN_SCALE, MAX_SCALE);
        Transform { position: Vec2::new((cw - w * k) * 0.5, (ch - h * k) * 0.5), scale: Vec2::new(k, k), ..Transform::default() }
    }

    /// The transform for a canvas that changed from `from` to `to`, for a source of `native`
    /// size (`None` before it has shown a picture).
    ///
    /// An item with a `fit` mode is placed by that mode on the new canvas ([`FitMode::place`]).
    /// Any other item keeps its center at the same place in proportion to the canvas, and its
    /// size is multiplied by the square root of the two sides' factors, so changing the canvas
    /// back gives the old layout. Without `native`, the top-left corner keeps its place in
    /// proportion instead. With the same canvas (`from` equal to `to`), only an item with a mode
    /// and a `native` size can move.
    pub fn relayout(self, fit: Option<FitMode>, native: Option<(u32, u32)>, from: (u32, u32), to: (u32, u32)) -> Transform {
        // The same canvas leaves an item without a mode exactly as it is.
        if fit.is_none() && from == to {
            return self;
        }
        let (fw, fh) = (from.0.max(1) as f32, from.1.max(1) as f32);
        let (tw, th) = (to.0.max(1) as f32, to.1.max(1) as f32);
        let (sx, sy) = (tw / fw, th / fh);
        let k = (sx * sy).sqrt();
        let Some(native) = native else {
            let position = Vec2::new(self.position.x * sx, self.position.y * sy);
            return Transform { position, scale: Vec2::new(self.scale.x * k, self.scale.y * k), ..self }.sanitized();
        };
        if let Some(mode) = fit {
            return mode.place(self, native, to);
        }
        let c = self.center(native);
        // The scale is held to its limits first, so the center stays in proportion at a limit.
        let scale = |s: f32| if (s * k).is_finite() { (s * k).clamp(MIN_SCALE, MAX_SCALE) } else { 1.0 };
        let t = Transform { scale: Vec2::new(scale(self.scale.x), scale(self.scale.y)), ..self };
        let size = t.size(native);
        let position = Vec2::new(c.x * sx - size.x * 0.5, c.y * sy - size.y * 0.5);
        Transform { position, ..t }.sanitized()
    }
}

/// How an item was sized to the canvas. An item keeps its mode until it is moved, resized,
/// turned or cropped, and a new canvas size places it by the same mode again.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FitMode {
    /// The whole source inside the canvas, centered, keeping its aspect ratio.
    Fit,
    /// The source covering the canvas, centered, keeping its aspect ratio; the parts past the
    /// canvas's edges are not drawn.
    Fill,
    /// The source stretched over the whole canvas.
    Stretch,
}

impl FitMode {
    /// `t` placed on a `canvas` by this mode for a source of `native` size. The crop stays; the
    /// rotation becomes 0. A source that needs a scale past the limits is drawn at the limit,
    /// centered on the canvas.
    pub fn place(self, t: Transform, native: (u32, u32), canvas: (u32, u32)) -> Transform {
        let cropped = t.crop.apply(native.0, native.1);
        match self {
            FitMode::Fit => Transform { crop: t.crop, ..Transform::fit(cropped, canvas) }.sanitized(),
            FitMode::Fill => Transform { crop: t.crop, ..Transform::fill(cropped, canvas) }.sanitized(),
            FitMode::Stretch => {
                let size = Vec2::new(canvas.0.max(1) as f32, canvas.1.max(1) as f32);
                let t = Transform { position: Vec2::ZERO, rotation: 0.0, ..t }.with_size(native, size);
                let drawn = t.size(native);
                // Rounding error on a size within the limits leaves the item at the origin.
                let margin = |canvas: f32, drawn: f32| if (canvas - drawn).abs() < 0.5 { 0.0 } else { (canvas - drawn) * 0.5 };
                Transform { position: Vec2::new(margin(size.x, drawn.x), margin(size.y, drawn.y)), ..t }.sanitized()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_never_leaves_an_empty_source() {
        let c = Crop { left: 100, top: 0, right: 100, bottom: 50 };
        assert_eq!(c.apply(1920, 1080), (1720, 1030));
        assert_eq!(c.apply(150, 40), (1, 1));
    }

    #[test]
    fn sanitizing_clamps_and_wraps() {
        let t = Transform { position: Vec2::new(f32::NAN, -1e9), scale: Vec2::new(0.0, f32::INFINITY), rotation: -90.0, crop: Crop::default() }
            .sanitized();
        assert_eq!(t.position, Vec2::new(0.0, -MAX_EXTENT));
        assert_eq!(t.scale, Vec2::new(MIN_SCALE, 1.0));
        assert_eq!(t.rotation, 270.0);
    }

    #[test]
    fn size_follows_crop_and_scale() {
        let t = Transform { scale: Vec2::new(0.5, 2.0), crop: Crop { left: 10, right: 10, ..Crop::default() }, ..Transform::default() };
        assert_eq!(t.size((120, 50)), Vec2::new(50.0, 100.0));
        let resized = t.with_size((120, 50), Vec2::new(200.0, 50.0));
        assert_eq!(resized.size((120, 50)), Vec2::new(200.0, 50.0));
    }

    #[test]
    fn rotated_hit_test() {
        let t = Transform { rotation: 90.0, ..Transform::default() };
        // A 200 x 20 bar rotated 90 degrees around (100, 10) stands vertically through x = 100.
        assert!(t.contains((200, 20), Vec2::new(100.0, 90.0)));
        assert!(!t.contains((200, 20), Vec2::new(10.0, 10.0)));
    }

    #[test]
    fn fit_keeps_aspect_and_centers() {
        let t = Transform::fit((1000, 1000), (1920, 1080));
        assert_eq!(t.size((1000, 1000)), Vec2::new(1080.0, 1080.0));
        assert_eq!(t.position, Vec2::new(420.0, 0.0));
    }

    #[test]
    fn fill_covers_the_canvas_and_centers() {
        let t = Transform::fill((1000, 1000), (1920, 1080));
        assert_eq!(t.size((1000, 1000)), Vec2::new(1920.0, 1920.0));
        assert_eq!(t.position, Vec2::new(0.0, -420.0));
        let t = Transform::fill((0, 0), (0, 0));
        assert!(t.position.is_finite() && t.scale.is_finite());
    }

    fn close(a: Vec2, b: Vec2) -> bool {
        (a.x - b.x).abs() < 0.01 && (a.y - b.y).abs() < 0.01
    }

    fn fit_tall(crop: Crop, n: (u32, u32)) -> Transform {
        Transform { crop, ..Transform::fit(crop.apply(n.0, n.1), (1080, 1920)) }
    }

    #[test]
    fn relayout_keeps_fit_fill_and_stretch() {
        let n = (1280, 720);
        let crop = Crop { left: 280, ..Crop::default() };
        let fit = FitMode::Fit.place(Transform { crop, ..Transform::default() }, n, (1920, 1080));
        assert_eq!(fit.relayout(Some(FitMode::Fit), Some(n), (1920, 1080), (1080, 1920)), fit_tall(crop, n));
        // A 16:9 source fit, filled or stretched over a 16:9 canvas has the same transform for
        // all three; the mode decides what a new canvas shape does.
        let wide = Transform::fit(n, (1920, 1080));
        assert_eq!(FitMode::Fill.place(wide, n, (1920, 1080)), wide);
        assert_eq!(wide.relayout(Some(FitMode::Fit), Some(n), (1920, 1080), (1080, 1920)), Transform::fit(n, (1080, 1920)));
        assert_eq!(wide.relayout(Some(FitMode::Fill), Some(n), (1920, 1080), (1080, 1920)), Transform::fill(n, (1080, 1920)));
        let r = wide.relayout(Some(FitMode::Stretch), Some(n), (1920, 1080), (1080, 1920));
        assert!(close(r.size(n), Vec2::new(1080.0, 1920.0)) && r.position == Vec2::ZERO, "{r:?}");
        // A turned item placed by a mode comes back upright.
        let turned = Transform { rotation: 180.0, ..fit };
        assert_eq!(turned.relayout(Some(FitMode::Fit), Some(n), (1920, 1080), (1080, 1920)), fit_tall(crop, n));
    }

    #[test]
    fn relayout_keeps_other_items_in_proportion_and_goes_back() {
        let n = (200, 100);
        let t = Transform { position: Vec2::new(100.0, 50.0), scale: Vec2::new(2.0, 1.0), rotation: 30.0, crop: Crop::default() };
        let r = t.relayout(None, Some(n), (1920, 1080), (3840, 2160));
        assert!(close(r.center(n), Vec2::new(600.0, 200.0)) && close(r.size(n), Vec2::new(800.0, 200.0)), "{r:?}");
        assert_eq!(r.rotation, 30.0);
        // A change of shape keeps the center in proportion; going back gives the old transform.
        let tall = t.relayout(None, Some(n), (1920, 1080), (1080, 1920));
        assert!(close(tall.center(n), Vec2::new(300.0 * 1080.0 / 1920.0, 100.0 * 1920.0 / 1080.0)), "{tall:?}");
        let back = tall.relayout(None, Some(n), (1080, 1920), (1920, 1080));
        assert!(close(back.position, t.position) && close(back.scale, t.scale), "{back:?}");
        // The same canvas leaves it exactly as it is.
        assert_eq!(t.relayout(None, Some(n), (1920, 1080), (1920, 1080)), t);
        // An item at the scale limit keeps its center in proportion.
        let big = Transform { scale: Vec2::new(MAX_SCALE, MAX_SCALE), ..Transform::default() };
        let r = big.relayout(None, Some((1, 1)), (1000, 1000), (2000, 2000));
        assert_eq!(r.scale, big.scale);
        assert!(close(r.center((1, 1)), Vec2::new(1000.0, 1000.0)), "{r:?}");
        // An item that only looks fit, with no mode, is kept in proportion too.
        let wide = Transform::fit((1280, 720), (1920, 1080));
        let r = wide.relayout(None, Some((1280, 720)), (1920, 1080), (1080, 1920));
        assert_ne!(r, Transform::fit((1280, 720), (1080, 1920)), "{r:?}");
    }

    #[test]
    fn relayout_without_a_size_moves_the_corner_in_proportion() {
        let t = Transform { position: Vec2::new(960.0, 540.0), scale: Vec2::new(0.5, 0.5), ..Transform::default() };
        for fit in [None, Some(FitMode::Fill)] {
            let r = t.relayout(fit, None, (1920, 1080), (1280, 720));
            assert!(close(r.position, Vec2::new(640.0, 360.0)) && close(r.scale, Vec2::new(1.0 / 3.0, 1.0 / 3.0)), "{r:?}");
        }
    }

    #[test]
    fn relayout_stays_in_range_for_hostile_sizes() {
        let t = Transform { position: Vec2::new(MAX_EXTENT, -MAX_EXTENT), scale: Vec2::new(MAX_SCALE, MIN_SCALE), ..Transform::default() };
        let cases = [(Some((0, 0)), (0, 0), (u32::MAX, 1)), (Some((u32::MAX, 1)), (1, u32::MAX), (16, 16)), (None, (16, 16), (8192, 8192))];
        for fit in [None, Some(FitMode::Fit), Some(FitMode::Fill), Some(FitMode::Stretch)] {
            for (n, from, to) in cases {
                let r = t.relayout(fit, n, from, to);
                assert!(r.check().is_ok(), "{fit:?} {r:?}");
            }
        }
    }

    #[test]
    fn a_source_past_the_scale_limit_stays_centered() {
        // A 1 x 1 source needs a scale of 1920 to fill a 1920 x 1080 canvas; the limit is 1000.
        let one = (1, 1);
        for mode in [FitMode::Fit, FitMode::Fill, FitMode::Stretch] {
            let t = mode.place(Transform::default(), one, (1920, 1080));
            assert!(t.check().is_ok(), "{mode:?} {t:?}");
            assert!(close(t.center(one), Vec2::new(960.0, 540.0)), "{mode:?} {t:?}");
        }
        assert_eq!(Transform::fill(one, (1920, 1080)).scale, Vec2::new(MAX_SCALE, MAX_SCALE));
        assert_eq!(Transform::fit(one, (1920, 1080)).scale, Vec2::new(MAX_SCALE, MAX_SCALE));
        // A long, thin source covering the canvas would reach past the position limit; it is
        // drawn smaller and stays centered.
        let thin = (65_536, 1);
        let t = FitMode::Fill.place(Transform::default(), thin, (8192, 8192));
        assert!(t.check().is_ok(), "{t:?}");
        // Positions this far out carry about 0.01 px of f32 rounding.
        let c = t.center(thin);
        assert!((c.x - 4096.0).abs() < 0.1 && (c.y - 4096.0).abs() < 0.1, "{t:?}");
        assert!(t.position.x >= -MAX_EXTENT, "{t:?}");
        // A source too large for the lower limit is drawn at it and stays in range.
        let huge = (u32::MAX, u32::MAX);
        let t = FitMode::Fit.place(Transform::default(), huge, (16, 16));
        assert_eq!(t.scale, Vec2::new(MIN_SCALE, MIN_SCALE));
        assert!(t.check().is_ok(), "{t:?}");
    }

    #[test]
    fn fit_modes_are_saved_by_name() {
        assert_eq!(serde_json::to_value(FitMode::Stretch).unwrap(), serde_json::json!("stretch"));
        assert_eq!(serde_json::from_value::<FitMode>(serde_json::json!("fill")).unwrap(), FitMode::Fill);
        assert!(serde_json::from_value::<FitMode>(serde_json::json!("cover")).is_err());
    }
}
