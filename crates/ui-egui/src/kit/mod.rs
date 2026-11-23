//! Interface pieces that depend only on egui, craft-ui, craft-theme and craft-menu and use no StudioCraft
//! types, so that other Craft apps can use them as they are.

pub mod banner;
pub mod chip;
pub mod keycap;
pub mod onair;
pub mod option_card;
pub mod scene_tile;
pub mod segmented;
pub mod tokens;

/// A corner radius token, in points, as egui's whole-point corner radius.
pub fn corner(px: f32) -> u8 {
    if px.is_finite() { px.round().clamp(0.0, f32::from(u8::MAX)) as u8 } else { 0 }
}
