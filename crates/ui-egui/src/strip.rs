//! The sizes of the dock strip under the preview: its height and the widths of its columns.
//!
//! The strip is 200 points tall, or 30% of the window's height when that is less, and never less
//! than 120 points. A height set by dragging the strip's top edge replaces the default and is kept
//! between 120 points and 45% of the window's height. Scenes and Sources have widths of their own
//! (300 and 340 points by default); the Audio Mixer, or the last shown dock when the mixer is
//! hidden, takes the rest. No column is narrower than 200 points while the strip has room for that,
//! and a 1 point divider separates two columns.

/// The default strip height and the share of the window it may take by default.
pub const DEFAULT_HEIGHT: f32 = 200.0;
pub const DEFAULT_SHARE: f32 = 0.30;
/// The least height, and the share of the window a dragged height may reach.
pub const MIN_HEIGHT: f32 = 120.0;
pub const MAX_SHARE: f32 = 0.45;
/// The narrowest a column is.
pub const MIN_WIDTH: f32 = 200.0;
/// The width of the line between two columns.
pub const DIVIDER: f32 = 1.0;
/// The default widths of Scenes and Sources, by dock index.
pub const DEFAULT_WIDTHS: [f32; 2] = [300.0, 340.0];

/// A saved size that can be used: finite and above zero. Zero is saved for a size the user has
/// not set.
pub fn usable(v: f32) -> Option<f32> {
    (v.is_finite() && v > 0.0).then_some(v)
}

/// The tallest the strip may be in a window `window` points tall.
pub fn max_height(window: f32) -> f32 {
    if window.is_finite() { (window * MAX_SHARE).max(MIN_HEIGHT) } else { MIN_HEIGHT }
}

/// The strip's height in a window `window` points tall, with the height the user set, if any.
pub fn height(window: f32, saved: f32) -> f32 {
    let window = if window.is_finite() { window.max(0.0) } else { 0.0 };
    match usable(saved) {
        Some(h) => h.clamp(MIN_HEIGHT, max_height(window)),
        None => (window * DEFAULT_SHARE).clamp(MIN_HEIGHT, DEFAULT_HEIGHT),
    }
}

/// The width of each shown dock (indexes 0 Scenes, 1 Sources, 2 Audio Mixer, in order) across
/// `total` points, from the saved widths of Scenes and Sources. The last column takes the room
/// the others leave; when that is less than [`MIN_WIDTH`], the other columns give up width down to
/// [`MIN_WIDTH`], in proportion to what they have above it. When even that is not enough, every
/// column gets the same share.
pub fn widths(total: f32, shown: &[u8], saved: [f32; 2]) -> Vec<f32> {
    let n = shown.len();
    if n == 0 {
        return Vec::new();
    }
    let total = if total.is_finite() { total.max(0.0) } else { 0.0 };
    let room = (total - DIVIDER * (n as f32 - 1.0)).max(0.0);
    if room < MIN_WIDTH * n as f32 {
        return vec![room / n as f32; n];
    }
    let widest = room - MIN_WIDTH * (n as f32 - 1.0);
    let mut out: Vec<f32> = shown[..n - 1]
        .iter()
        .map(|i| {
            let k = usize::from(*i).min(1);
            usable(saved[k]).unwrap_or(DEFAULT_WIDTHS[k]).clamp(MIN_WIDTH, widest)
        })
        .collect();
    let fixed: f32 = out.iter().sum();
    let short = MIN_WIDTH - (room - fixed);
    if short > 0.0 {
        let spare: f32 = out.iter().map(|w| w - MIN_WIDTH).sum();
        for w in &mut out {
            *w -= short * (*w - MIN_WIDTH) / spare;
        }
    }
    let used: f32 = out.iter().sum();
    out.push(room - used);
    out
}

/// The widths after the divider right of column `k` moved `delta` points: column `k` changes by
/// `delta` and the column right of it by the opposite, each staying at least [`MIN_WIDTH`] wide
/// (or as wide as it is, when that is already less).
pub fn drag(widths: &[f32], k: usize, delta: f32) -> Vec<f32> {
    let mut out = widths.to_vec();
    if k + 1 >= out.len() || !delta.is_finite() {
        return out;
    }
    let (a, b) = (out[k], out[k + 1]);
    let delta = delta.clamp(-(a - MIN_WIDTH).max(0.0), (b - MIN_WIDTH).max(0.0));
    out[k] = a + delta;
    out[k + 1] = b - delta;
    out
}

/// The saved widths of Scenes and Sources after the divider right of shown column `k` moved, with
/// `moved` the drawn widths that [`drag`] returned. Only the two columns beside that divider change;
/// the last shown column takes the rest and keeps no width, and every other column keeps the width
/// that was saved for it.
pub fn save(saved: [f32; 2], shown: &[u8], moved: &[f32], k: usize) -> [f32; 2] {
    let mut out = saved;
    let last = shown.len().saturating_sub(1);
    for j in [k, k + 1] {
        if j < last
            && let (Some(i), Some(w)) = (shown.get(j), moved.get(j))
        {
            out[usize::from(*i).min(1)] = *w;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_height_is_200_or_30_percent_and_at_least_120() {
        assert_eq!(height(800.0, 0.0), 200.0);
        assert_eq!(height(1000.0, 0.0), 200.0);
        assert!((height(480.0, 0.0) - 144.0).abs() < 0.001);
        assert_eq!(height(300.0, 0.0), 120.0);
        assert_eq!(height(0.0, 0.0), 120.0);
        assert_eq!(height(f32::NAN, 0.0), 120.0);
    }

    #[test]
    fn a_saved_height_replaces_the_default_within_its_bounds() {
        assert_eq!(height(800.0, 260.0), 260.0);
        assert_eq!(height(800.0, 50.0), 120.0);
        assert_eq!(height(800.0, 700.0), 360.0, "45% of 800");
        assert_eq!(height(200.0, 150.0), 120.0, "the least height wins in a short window");
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -5.0, 0.0] {
            assert_eq!(height(800.0, bad), 200.0, "{bad} reads as no saved height");
        }
    }

    #[test]
    fn at_1280_the_columns_are_300_340_and_the_rest() {
        let w = widths(1280.0, &[0, 1, 2], [0.0, 0.0]);
        assert_eq!(w, [300.0, 340.0, 638.0]);
        assert_eq!(w.iter().sum::<f32>() + 2.0, 1280.0);
    }

    #[test]
    fn a_hidden_dock_gives_its_width_to_the_others() {
        assert_eq!(widths(1280.0, &[0, 2], [0.0, 0.0]), [300.0, 979.0]);
        assert_eq!(widths(1280.0, &[1, 2], [250.0, 0.0]), [340.0, 939.0], "Sources keeps its own width");
        assert_eq!(widths(1280.0, &[0, 1], [0.0, 0.0]), [300.0, 979.0], "without the mixer Sources takes the rest");
        assert_eq!(widths(1280.0, &[1], [0.0, 0.0]), [1280.0]);
        assert!(widths(1280.0, &[], [0.0, 0.0]).is_empty());
    }

    #[test]
    fn a_narrow_window_keeps_every_column_at_least_200() {
        let w = widths(760.0, &[0, 1, 2], [0.0, 0.0]);
        assert!((w.iter().sum::<f32>() + 2.0 - 760.0).abs() < 0.01, "{w:?}");
        for c in &w {
            assert!(*c >= MIN_WIDTH - 0.01, "{w:?}");
        }
        assert!(w[1] > w[0], "Sources stays wider than Scenes: {w:?}");
        let tiny = widths(400.0, &[0, 1, 2], [0.0, 0.0]);
        assert_eq!(tiny.len(), 3);
        assert!((tiny[0] - 398.0 / 3.0).abs() < 0.01 && tiny.windows(2).all(|p| p[0] == p[1]), "{tiny:?}");
    }

    #[test]
    fn saved_widths_are_used_and_bad_ones_are_not() {
        assert_eq!(widths(1280.0, &[0, 1, 2], [250.0, 400.0]), [250.0, 400.0, 628.0]);
        assert_eq!(widths(1280.0, &[0, 1, 2], [f32::NAN, -1.0]), [300.0, 340.0, 638.0]);
        assert_eq!(widths(1280.0, &[0, 1, 2], [10.0, 0.0]), [200.0, 340.0, 738.0], "no column under 200");
        let huge = widths(1280.0, &[0, 1, 2], [1.0e30, 5000.0]);
        assert!(huge.iter().all(|w| w.is_finite() && *w >= MIN_WIDTH - 0.01), "{huge:?}");
        assert!((huge.iter().sum::<f32>() + 2.0 - 1280.0).abs() < 0.5, "{huge:?}");
        assert!(widths(f32::NAN, &[0, 1, 2], [0.0, 0.0]).iter().all(|w| *w == 0.0));
    }

    #[test]
    fn a_dragged_divider_moves_width_between_its_two_columns() {
        let w = [300.0, 340.0, 638.0];
        assert_eq!(drag(&w, 0, 40.0), [340.0, 300.0, 638.0]);
        assert_eq!(drag(&w, 1, -40.0), [300.0, 300.0, 678.0]);
        assert_eq!(drag(&w, 0, 500.0), [440.0, 200.0, 638.0], "the right column stops at 200");
        assert_eq!(drag(&w, 0, -500.0), [200.0, 440.0, 638.0], "the left column stops at 200");
        assert_eq!(drag(&w, 2, 10.0), w, "the last column has no divider on its right");
        assert_eq!(drag(&w, 0, f32::NAN), w);
    }

    #[test]
    fn a_drag_saves_only_the_columns_beside_its_divider() {
        let shown = [0, 1, 2];
        let w = widths(760.0, &shown, [0.0, 0.0]);
        assert!(w[0] < DEFAULT_WIDTHS[0], "Scenes is squeezed at 760: {w:?}");
        let moved = drag(&w, 1, -20.0);
        let out = save([0.0, 0.0], &shown, &moved, 1);
        assert_eq!(out[0], 0.0, "Scenes was not resized and stays unset: {out:?}");
        assert_eq!(out[1], moved[1]);
        let moved = drag(&w, 0, 10.0);
        assert_eq!(save([0.0, 0.0], &shown, &moved, 0), [moved[0], moved[1]], "both columns beside divider 0");
        let out = save([250.0, 0.0], &[0, 2], &drag(&[300.0, 979.0], 0, 15.0), 0);
        assert_eq!(out, [315.0, 0.0], "the last column keeps no width");
        assert_eq!(save([250.0, 400.0], &[], &[], 0), [250.0, 400.0]);
    }
}
