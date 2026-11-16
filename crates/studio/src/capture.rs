//! Still pictures of the canvas as PNG files, for render previews.

use image::imageops::FilterType;
use image::{ExtendedColorType, ImageBuffer, ImageEncoder, Rgba};

use crate::{Capture, StudioError};

/// Largest side of an encoded picture.
pub const MAX_PNG_SIDE: u32 = 4096;

/// The size a `width` x `height` picture is scaled to so that its longer side is at most
/// `max_side` (0 means full size), keeping its aspect. Neither side is capped above
/// [`MAX_PNG_SIDE`], and neither side drops below 1.
pub fn scaled_size(width: u32, height: u32, max_side: u32) -> (u32, u32) {
    let limit = if max_side == 0 { MAX_PNG_SIDE } else { max_side.min(MAX_PNG_SIDE) };
    let longer = width.max(height).max(1);
    if longer <= limit {
        return (width.max(1), height.max(1));
    }
    let scale = f64::from(limit) / f64::from(longer);
    let side = |v: u32| ((f64::from(v) * scale).round() as u32).clamp(1, limit);
    (side(width), side(height))
}

/// Encode `capture` as PNG, scaled down so its longer side is at most `max_side` (0 means full
/// size; at most [`MAX_PNG_SIDE`] either way).
pub fn png(capture: &Capture, max_side: u32) -> Result<Vec<u8>, StudioError> {
    let (w, h) = (capture.width, capture.height);
    let expected = u64::from(w).checked_mul(u64::from(h)).and_then(|n| n.checked_mul(4));
    if w == 0 || h == 0 || expected != Some(capture.rgba.len() as u64) {
        return Err(StudioError::Image(format!("a {w} x {h} picture cannot hold {} bytes", capture.rgba.len())));
    }
    let Some(full) = ImageBuffer::<Rgba<u8>, &[u8]>::from_raw(w, h, capture.rgba.as_slice()) else {
        return Err(StudioError::Image(format!("a {w} x {h} picture cannot hold {} bytes", capture.rgba.len())));
    };
    let (tw, th) = scaled_size(w, h, max_side);
    let mut out = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut out);
    let written = if (tw, th) == (w, h) {
        encoder.write_image(full.as_raw(), w, h, ExtendedColorType::Rgba8)
    } else {
        let small = image::imageops::resize(&full, tw, th, FilterType::Triangle);
        encoder.write_image(small.as_raw(), tw, th, ExtendedColorType::Rgba8)
    };
    written.map_err(|e| StudioError::Image(e.to_string()))?;
    Ok(out)
}
