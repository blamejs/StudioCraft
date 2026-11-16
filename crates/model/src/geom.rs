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
    /// aspect ratio.
    pub fn fit(native: (u32, u32), canvas: (u32, u32)) -> Transform {
        let (w, h) = (native.0.max(1) as f32, native.1.max(1) as f32);
        let (cw, ch) = (canvas.0.max(1) as f32, canvas.1.max(1) as f32);
        let k = (cw / w).min(ch / h);
        Transform { position: Vec2::new((cw - w * k) * 0.5, (ch - h * k) * 0.5), scale: Vec2::new(k, k), ..Transform::default() }
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
}
