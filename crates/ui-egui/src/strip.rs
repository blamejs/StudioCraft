//! The sizes of the docks' regions: the dock strip under the preview, its height and the widths of
//! its columns, and the columns left and right of the preview; and the handles that change them.
//!
//! The strip is 200 points tall, or 30% of the window's height when that is less, and never less
//! than 120 points. A height set by dragging the strip's top edge replaces the default. The strip
//! is never so tall that less than 270 points of height are left above it for the preview, unless
//! 120 points already leave less. Every shown dock but the last one has a width of its own
//! (Scenes 300, Sources 340, Audio Mixer 400 and Application Audio 360 points by default); the last
//! shown dock takes the rest. No column is narrower than 200 points while the strip has room for
//! that, and a 1 point divider separates two columns.
//!
//! The left column is 256 points wide and the right column 296 by default; a width set by
//! dragging a column's inner edge is kept between 200 and 480 points. The docks in a column share
//! its height equally, with a 1 point line between two of them. In a window narrower than 900
//! points, or when the columns would leave the preview less than 480 points of width, the columns'
//! docks are drawn in the strip instead.
//!
//! [`Handles`] shows the strip's top edge, its dividers and the columns' edges as the framework's
//! splitters.

use craft_behavior::divider::{Give, Segment, resize, travel};
use craft_behavior::{Axis, Metrics, PartKey, PressPhase, Sense};
use craft_ui::contract::{Access, Common, Component, Cx, NoValue, Outcome, PaintUi, PartsCx, Pointer as _, Props as _, Role};
use craft_ui::docking::divider::{self, Divider};
use craft_ui::docking::{DockSizes, Splitter};
use craft_ui::runtime::focus::KeyUse;
use craft_ui::style::{ComponentStyle, StyleCx};
use egui::{Rect, Response};

/// The default strip height and the share of the window it may take by default.
pub const DEFAULT_HEIGHT: f32 = 200.0;
pub const DEFAULT_SHARE: f32 = 0.30;
/// The least height.
pub const MIN_HEIGHT: f32 = 120.0;
/// The height the strip leaves above it for the preview, as far as it can.
pub const CANVAS_MIN_HEIGHT: f32 = 270.0;
/// The narrowest a column is.
pub const MIN_WIDTH: f32 = 200.0;
/// The width of the line between two columns.
pub const DIVIDER: f32 = 1.0;
/// The default widths of Scenes, Sources, Audio Mixer, Application Audio and Health, by dock index.
pub const DEFAULT_WIDTHS: [f32; crate::store::DOCK_COUNT] = [300.0, 340.0, 400.0, 360.0, 300.0];

/// The default width of dock `i`; an index past the last dock gets the last dock's width.
pub fn default_width(i: usize) -> f32 {
    DEFAULT_WIDTHS.get(i).or(DEFAULT_WIDTHS.last()).copied().unwrap_or(MIN_WIDTH)
}

/// A saved size that can be used: finite and above zero. Zero is saved for a size the user has
/// not set.
pub fn usable(v: f32) -> Option<f32> {
    (v.is_finite() && v > 0.0).then_some(v)
}

/// The tallest the strip may be when it and the preview above it share `body` points of height:
/// what leaves the preview [`CANVAS_MIN_HEIGHT`], and never less than [`MIN_HEIGHT`].
pub fn max_height(body: f32) -> f32 {
    if body.is_finite() { (body - CANVAS_MIN_HEIGHT).max(MIN_HEIGHT) } else { MIN_HEIGHT }
}

/// The strip's height in a window `window` points tall where it and the preview share `body`
/// points, with the height the user set, if any.
pub fn height(window: f32, body: f32, saved: f32) -> f32 {
    let window = if window.is_finite() { window.max(0.0) } else { 0.0 };
    let most = max_height(body);
    match usable(saved) {
        Some(h) => h.clamp(MIN_HEIGHT, most),
        None => (window * DEFAULT_SHARE).clamp(MIN_HEIGHT, DEFAULT_HEIGHT).min(most),
    }
}

/// The default widths of the left and right columns.
pub const LEFT_WIDTH: f32 = 256.0;
pub const RIGHT_WIDTH: f32 = 296.0;
/// The widest a column is.
pub const MAX_COLUMN_WIDTH: f32 = 480.0;
/// A window narrower than this draws the columns' docks in the strip.
pub const NARROW: f32 = 900.0;
/// The least width the columns leave for the preview; with less, their docks are drawn in the
/// strip.
pub const CANVAS_MIN_WIDTH: f32 = 480.0;

/// A column's width: the saved one, or `default` when none is saved, kept between [`MIN_WIDTH`]
/// and [`MAX_COLUMN_WIDTH`].
pub fn column_width(saved: f32, default: f32) -> f32 {
    usable(saved).unwrap_or(default).clamp(MIN_WIDTH, MAX_COLUMN_WIDTH)
}

/// Whether the columns, `columns` points wide together with their edges, are drawn in the strip
/// instead in a window `width` points wide.
pub fn collapsed(width: f32, columns: f32) -> bool {
    columns > 0.0 && (!width.is_finite() || width < NARROW || width - columns < CANVAS_MIN_WIDTH)
}

/// The heights of `n` docks sharing a column `total` points tall, with a [`DIVIDER`] between two.
pub fn stack(total: f32, n: usize) -> Vec<f32> {
    if n == 0 {
        return Vec::new();
    }
    let total = if total.is_finite() { total.max(0.0) } else { 0.0 };
    let room = (total - DIVIDER * (n as f32 - 1.0)).max(0.0);
    vec![room / n as f32; n]
}

/// The widest a column may be dragged in a body `body` points wide, when the columns with their
/// lines take `columns` points of it and this column with its line `own`: [`MAX_COLUMN_WIDTH`], or
/// less where a wider column would leave the preview under [`CANVAS_MIN_WIDTH`] and draw the
/// columns in the strip.
pub fn column_most(body: f32, columns: f32, own: f32) -> f32 {
    let room = if body.is_finite() { body - CANVAS_MIN_WIDTH - (columns - own) - DIVIDER } else { MIN_WIDTH };
    room.min(MAX_COLUMN_WIDTH)
}

/// The inner edge of a column whose docks fill `area` (the left column's right edge when `left`,
/// the right column's left edge otherwise), centered on the 1 point line beside the docks and
/// `reach` points thick. It moves until the column is [`MIN_WIDTH`] or `most` wide; a column
/// already outside that range does not move further out.
pub fn column_edge(area: Rect, left: bool, reach: f32, most: f32) -> Divider {
    let w = area.width();
    let (shrink, grow) = (w - MIN_WIDTH.min(w), most.max(w) - w);
    if left {
        let at = area.right() + DIVIDER / 2.0;
        Divider::new(Axis::X, at, area, (at - shrink, at + grow), reach)
    } else {
        let at = area.left() - DIVIDER / 2.0;
        Divider::new(Axis::X, at, area, (at - grow, at + shrink), reach)
    }
}

/// A column's width after its edge, `edge`, moved to `at`, at most `most`.
pub fn column_drag(edge: &Divider, width: f32, left: bool, at: f32, most: f32) -> f32 {
    let moved = if at.is_finite() { at - edge.at } else { 0.0 };
    let w = if left { width + moved } else { width - moved };
    w.clamp(MIN_WIDTH.min(width), most.max(width))
}

/// The width of each shown dock (indexes 0 Scenes, 1 Sources, 2 Audio Mixer, 3 Application Audio,
/// from left to right) across `total` points, from the saved widths by dock index. The last column
/// takes the room the others leave; when that is less than [`MIN_WIDTH`], the other columns give
/// up width down to [`MIN_WIDTH`], in proportion to what they have above it. When even that is not
/// enough, every column gets the same share.
pub fn widths<const N: usize>(total: f32, shown: &[u8], saved: [f32; N]) -> Vec<f32> {
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
            let k = usize::from(*i);
            saved.get(k).copied().and_then(usable).unwrap_or_else(|| default_width(k)).clamp(MIN_WIDTH, widest)
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

/// The columns as a row of the framework's divider segments: each gives way down to [`MIN_WIDTH`],
/// or not at all when it is already narrower.
fn segments(widths: &[f32]) -> Vec<Segment> {
    widths.iter().map(|w| Segment::unbounded(*w, MIN_WIDTH.min(*w))).collect()
}

/// The widths after the divider right of column `k` moved `delta` points: column `k` changes by
/// `delta` and the column right of it by the opposite, each staying at least [`MIN_WIDTH`] wide
/// (or as wide as it is, when that is already less). The framework's divider rule for the two
/// columns beside a divider clamps the move.
pub fn drag(widths: &[f32], k: usize, delta: f32) -> Vec<f32> {
    if k + 1 >= widths.len() || !delta.is_finite() {
        return widths.to_vec();
    }
    let end: f32 = widths.iter().take(k + 1).sum();
    resize(&segments(widths), k, end + delta, Give::Pair)
}

/// The dividers between the columns `widths` wide that the strip lays out in `area` from its left
/// edge, in order: each centered on its 1 point line, `reach` points thick, and moving as far as
/// [`drag`] lets it.
pub fn dividers(area: Rect, widths: &[f32], reach: f32) -> Vec<Divider> {
    let row = segments(widths);
    let mut x = area.left();
    let mut out = Vec::with_capacity(widths.len().saturating_sub(1));
    for (k, w) in widths.iter().enumerate().take(widths.len().saturating_sub(1)) {
        x += w;
        let at = x + DIVIDER / 2.0;
        let (lo, hi) = travel(&row, k, Give::Pair);
        out.push(Divider::new(Axis::X, at, area, (at + lo, at + hi), reach));
        x += DIVIDER;
    }
    out
}

/// The strip's top edge for the strip in `area`, centered on the 1 point line at the top of the
/// strip and `reach` points thick. It moves up until the strip is `most` points tall and down
/// until it is [`MIN_HEIGHT`] tall; a strip already outside that range does not move further out.
pub fn edge(area: Rect, most: f32, reach: f32) -> Divider {
    let at = area.top() + DIVIDER / 2.0;
    let h = area.height();
    let (least, most) = (MIN_HEIGHT.min(h), most.max(h));
    Divider::new(Axis::Y, at, area, (at - (most - h), at + (h - least)), reach)
}

/// How far down and to the right each dock's window opens from the one before it.
pub const CASCADE: f32 = 24.0;

/// Where the window of dock `k` opens in the main window `bounds` when no place was saved for it:
/// centered, moved [`CASCADE`] points down and to the right per dock index, and inside the main
/// window where it fits.
pub fn window_place(bounds: Rect, size: egui::Vec2, k: usize) -> egui::Pos2 {
    let step = CASCADE * k.min(crate::store::DOCK_COUNT) as f32;
    let at = bounds.center() - size / 2.0 + egui::vec2(step, step);
    at.clamp(bounds.min, (bounds.max - size).max(bounds.min))
}

/// The saved widths by dock index after the divider right of shown column `k` moved, with `moved`
/// the drawn widths that [`drag`] returned. Only the two columns beside that divider change; the
/// last shown column takes the rest and keeps no width, and every other column keeps the width that
/// was saved for it.
pub fn save<const N: usize>(saved: [f32; N], shown: &[u8], moved: &[f32], k: usize) -> [f32; N] {
    let mut out = saved;
    let last = shown.len().saturating_sub(1);
    for j in [k, k + 1] {
        if j < last
            && let (Some(i), Some(w)) = (shown.get(j), moved.get(j))
            && let Some(o) = out.get_mut(usize::from(*i))
        {
            *o = *w;
        }
    }
    out
}

/// The parts of [`Handles`].
const HANDLE: PartKey = PartKey::new("handle");

/// The style of [`Handles`]: how far an arrow key moves a handle, and with Shift.
#[derive(Clone, Debug, PartialEq, ComponentStyle)]
#[non_exhaustive]
pub struct HandlesStyle {
    #[style(metrics)]
    metrics: Metrics,
    step: f32,
    jump: f32,
}

/// What [`Handles`] reports in a pass.
#[derive(Debug)]
pub struct HandlesOutput {
    pub response: Response,
    /// The handle that was dragged or moved by the keys, by its index, and the coordinate it was
    /// moved to, within its range. `None` when no handle moved.
    pub moved: Option<(usize, f32)>,
}

impl Outcome for HandlesOutput {
    type Value = NoValue;

    fn response(&self) -> &Response {
        &self.response
    }
}

/// Handles on the strip's top edge or its dividers: each is a framework splitter that the pointer
/// drags and that is a Tab stop the arrow keys move, named `name` for assistive technology. The
/// handles report where they were moved to; the strip applies the move.
#[derive(craft_ui::contract::Component, craft_ui::contract::Props)]
pub struct Handles {
    #[common]
    common: Common,
    dividers: Vec<Divider>,
    name: String,
    moved: Option<(usize, f32)>,
}

impl Handles {
    /// Handles named `name` over `dividers`, which are in the strip's physical coordinates.
    pub fn new(name: impl Into<String>, dividers: Vec<Divider>) -> Self {
        let name = name.into();
        Handles { common: Common::new(), dividers, name: name.clone(), moved: None }.label(name).ltr_axis()
    }

    fn keyed(&self) -> Vec<(u64, Divider)> {
        self.dividers.iter().enumerate().map(|(k, d)| (k as u64, *d)).collect()
    }
}

impl Component for Handles {
    const ROLE: Role = Role::GROUP;
    const KEYS: KeyUse = KeyUse::SPLITTER;
    const OVERLAYS: bool = false;
    type Style = HandlesStyle;
    type Output = HandlesOutput;

    fn style(cx: &StyleCx<'_>, _: HandlesStyleKey) -> HandlesStyle {
        let sizes = DockSizes::style(cx);
        HandlesStyle { metrics: Metrics::new(), step: sizes.step, jump: sizes.jump }
    }

    fn sense(&self) -> Sense {
        Sense::empty()
    }

    fn parts(&mut self, rect: Rect, cx: &mut PartsCx<'_, Self>) {
        for (k, d) in self.dividers.iter().enumerate() {
            divider::declare(cx, rect, HANDLE, k as u64, d);
        }
    }

    fn update(&mut self, cx: &mut Cx<'_, Self>) {
        for (k, d) in self.dividers.iter().enumerate() {
            // The line moves by as much as the pointer did, from the press on the first move past the drag
            // threshold and from the last pass after that, so a press beside the line does not make it jump.
            let moved = match cx.part(HANDLE.at(k as u64)).press.phase {
                PressPhase::DragStart { start, pos, .. } => pos - start.origin,
                PressPhase::Drag { delta, .. } => delta,
                _ => continue,
            };
            let at = d.at + if d.axis == Axis::X { moved.x } else { moved.y };
            if at.is_finite() {
                let at = at.clamp(d.lo.min(d.at), d.hi.max(d.at));
                if at != d.at {
                    self.moved = Some((k, at));
                }
            }
        }
        let (step, jump) = (cx.style.step, cx.style.jump);
        if let Some((k, at)) = divider::keys(cx, HANDLE, &self.keyed(), step, jump)
            && at.is_finite()
            && self.dividers.get(k as usize).is_some_and(|d| at != d.at)
        {
            self.moved = Some((k as usize, at));
        }
    }

    fn paint_over(&mut self, ui: &mut PaintUi<'_>, _: &Cx<'_, Self>) {
        for (k, d) in self.dividers.iter().enumerate() {
            ui.face(HANDLE.at(k as u64), Splitter::new(d.axis));
        }
    }

    fn access(&self, access: &mut Access, cx: &Cx<'_, Self>) {
        for (k, d) in self.dividers.iter().enumerate() {
            divider::describe(access, HANDLE, k as u64, d, &self.name, cx.style.step);
        }
    }

    fn finish(self, cx: Cx<'_, Self>) -> HandlesOutput {
        HandlesOutput { response: cx.response.clone(), moved: self.moved }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_height_is_200_or_30_percent_and_at_least_120() {
        assert_eq!(height(800.0, 744.0, 0.0), 200.0);
        assert_eq!(height(1000.0, 944.0, 0.0), 200.0);
        assert!((height(480.0, 424.0, 0.0) - 144.0).abs() < 0.001);
        assert_eq!(height(300.0, 244.0, 0.0), 120.0);
        assert_eq!(height(0.0, 0.0, 0.0), 120.0);
        assert_eq!(height(f32::NAN, f32::NAN, 0.0), 120.0);
        assert_eq!(height(800.0, 400.0, 0.0), 130.0, "the default leaves the preview 270 points too");
    }

    #[test]
    fn a_saved_height_replaces_the_default_within_its_bounds() {
        assert_eq!(height(800.0, 744.0, 260.0), 260.0);
        assert_eq!(height(800.0, 744.0, 50.0), 120.0);
        assert_eq!(height(800.0, 744.0, 700.0), 474.0, "the preview keeps 270 of the 744 points");
        assert_eq!(height(480.0, 424.0, 300.0), 154.0, "and at 760 by 480");
        assert_eq!(height(200.0, 144.0, 150.0), 120.0, "the least height wins in a short window");
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -5.0, 0.0] {
            assert_eq!(height(800.0, 744.0, bad), 200.0, "{bad} reads as no saved height");
        }
        assert_eq!(height(800.0, f32::INFINITY, 700.0), 120.0, "a body that is not finite allows only the least height");
    }

    #[test]
    fn columns_keep_their_width_and_collapse_into_the_strip_in_a_narrow_window() {
        assert_eq!((column_width(0.0, LEFT_WIDTH), column_width(0.0, RIGHT_WIDTH)), (256.0, 296.0));
        assert_eq!(column_width(320.0, LEFT_WIDTH), 320.0);
        for (saved, want) in [(50.0, MIN_WIDTH), (5000.0, MAX_COLUMN_WIDTH), (f32::NAN, 256.0), (f32::INFINITY, 256.0), (-1.0, 256.0)] {
            assert_eq!(column_width(saved, LEFT_WIDTH), want, "{saved}");
        }
        assert!(!collapsed(1280.0, 257.0 + 297.0), "1280 holds both default columns");
        assert!(!collapsed(1034.0, 257.0 + 297.0) && collapsed(1033.0, 257.0 + 297.0), "the preview keeps 480 points");
        assert!(!collapsed(1024.0, 257.0), "1024 holds the left column");
        assert!(!collapsed(900.0, 257.0) && collapsed(899.0, 257.0) && collapsed(760.0, 257.0), "760 by 480 draws the columns in the strip");
        assert!(!collapsed(760.0, 0.0), "without a column there is nothing to collapse");
        assert!(collapsed(f32::NAN, 257.0));
    }

    #[test]
    fn a_column_s_docks_share_its_height() {
        assert_eq!(stack(401.0, 2), [200.0, 200.0]);
        assert_eq!(stack(300.0, 1), [300.0]);
        assert!(stack(300.0, 0).is_empty());
        assert_eq!(stack(1.0, 3), [0.0, 0.0, 0.0], "no negative height");
        assert_eq!(stack(f32::NAN, 2), [0.0, 0.0]);
    }

    #[test]
    fn a_column_s_inner_edge_moves_between_its_least_and_most_width() {
        let left = Rect::from_min_size(egui::pos2(0.0, 32.0), egui::vec2(256.0, 600.0));
        let e = column_edge(left, true, 8.0, MAX_COLUMN_WIDTH);
        assert_eq!((e.axis, e.at, e.lo, e.hi), (Axis::X, 256.5, 200.5, 480.5));
        assert_eq!(e.bar, Rect::from_min_max(egui::pos2(252.5, 32.0), egui::pos2(260.5, 632.0)));
        assert_eq!(column_drag(&e, 256.0, true, 300.5, MAX_COLUMN_WIDTH), 300.0, "dragging the left column's edge right widens it");
        assert_eq!(column_drag(&e, 256.0, true, 0.0, MAX_COLUMN_WIDTH), MIN_WIDTH);
        assert_eq!(column_drag(&e, 256.0, true, f32::NAN, MAX_COLUMN_WIDTH), 256.0);
        let right = Rect::from_min_size(egui::pos2(984.0, 32.0), egui::vec2(296.0, 600.0));
        let e = column_edge(right, false, 8.0, MAX_COLUMN_WIDTH);
        assert_eq!((e.at, e.lo, e.hi), (983.5, 799.5, 1079.5));
        assert_eq!(column_drag(&e, 296.0, false, 963.5, MAX_COLUMN_WIDTH), 316.0, "dragging the right column's edge left widens it");
        assert_eq!(column_drag(&e, 296.0, false, 5000.0, MAX_COLUMN_WIDTH), MIN_WIDTH);
    }

    #[test]
    fn a_column_grows_only_while_the_preview_keeps_its_least_width() {
        // A 1280 point window has room for both columns at their widest.
        assert_eq!(column_most(1280.0, 257.0 + 297.0, 257.0), MAX_COLUMN_WIDTH);
        // At 900 points the left column alone stops at 419 points: 900 - 480 for the preview - 1 for its line.
        assert_eq!(column_most(900.0, 257.0, 257.0), 419.0);
        // With the right column shown too, its 297 points come off as well.
        assert_eq!(column_most(1200.0, 257.0 + 297.0, 257.0), 422.0);
        assert_eq!(column_most(f32::NAN, 257.0, 257.0), MIN_WIDTH);
        let area = Rect::from_min_size(egui::pos2(0.0, 32.0), egui::vec2(256.0, 600.0));
        let most = column_most(900.0, 257.0, 257.0);
        let e = column_edge(area, true, 8.0, most);
        assert_eq!(e.hi, 419.5);
        assert_eq!(column_drag(&e, 256.0, true, 700.0, most), 419.0, "a drag past the limit stops at it");
        assert!(!collapsed(900.0, 419.0 + DIVIDER), "the widest column still leaves the preview its width");
    }

    #[test]
    fn at_1280_the_columns_are_300_340_and_the_rest() {
        let w = widths(1280.0, &[0, 1, 2], [0.0, 0.0, 0.0]);
        assert_eq!(w, [300.0, 340.0, 638.0]);
        assert_eq!(w.iter().sum::<f32>() + 2.0, 1280.0);
    }

    #[test]
    fn a_hidden_dock_gives_its_width_to_the_others() {
        assert_eq!(widths(1280.0, &[0, 2], [0.0, 0.0, 0.0]), [300.0, 979.0]);
        assert_eq!(widths(1280.0, &[1, 2], [250.0, 0.0, 0.0]), [340.0, 939.0], "Sources keeps its own width");
        assert_eq!(widths(1280.0, &[0, 1], [0.0, 0.0, 0.0]), [300.0, 979.0], "without the mixer Sources takes the rest");
        assert_eq!(widths(1280.0, &[1], [0.0, 0.0, 0.0]), [1280.0]);
        assert!(widths(1280.0, &[], [0.0, 0.0, 0.0]).is_empty());
    }

    #[test]
    fn a_narrow_window_keeps_every_column_at_least_200() {
        let w = widths(760.0, &[0, 1, 2], [0.0, 0.0, 0.0]);
        assert!((w.iter().sum::<f32>() + 2.0 - 760.0).abs() < 0.01, "{w:?}");
        for c in &w {
            assert!(*c >= MIN_WIDTH - 0.01, "{w:?}");
        }
        assert!(w[1] > w[0], "Sources stays wider than Scenes: {w:?}");
        let tiny = widths(400.0, &[0, 1, 2], [0.0, 0.0, 0.0]);
        assert_eq!(tiny.len(), 3);
        assert!((tiny[0] - 398.0 / 3.0).abs() < 0.01 && tiny.windows(2).all(|p| p[0] == p[1]), "{tiny:?}");
    }

    #[test]
    fn saved_widths_are_used_and_bad_ones_are_not() {
        assert_eq!(widths(1280.0, &[0, 1, 2], [250.0, 400.0, 0.0]), [250.0, 400.0, 628.0]);
        assert_eq!(widths(1280.0, &[0, 1, 2], [f32::NAN, -1.0, 0.0]), [300.0, 340.0, 638.0]);
        assert_eq!(widths(1280.0, &[0, 1, 2], [10.0, 0.0, 0.0]), [200.0, 340.0, 738.0], "no column under 200");
        assert_eq!(widths(1280.0, &[0, 1, 2], [0.0, 0.0, 900.0]), [300.0, 340.0, 638.0], "the last column's width is not used");
        let huge = widths(1280.0, &[0, 1, 2], [1.0e30, 5000.0, 0.0]);
        assert!(huge.iter().all(|w| w.is_finite() && *w >= MIN_WIDTH - 0.01), "{huge:?}");
        assert!((huge.iter().sum::<f32>() + 2.0 - 1280.0).abs() < 0.5, "{huge:?}");
        assert!(widths(f32::NAN, &[0, 1, 2], [0.0, 0.0, 0.0]).iter().all(|w| *w == 0.0));
    }

    #[test]
    fn a_drag_saves_only_the_columns_beside_its_divider() {
        let shown = [0, 1, 2];
        let w = widths(760.0, &shown, [0.0, 0.0, 0.0]);
        assert!(w[0] < DEFAULT_WIDTHS[0], "Scenes is squeezed at 760: {w:?}");
        let moved = drag(&w, 1, -20.0);
        let out = save([0.0, 0.0, 0.0], &shown, &moved, 1);
        assert_eq!(out[0], 0.0, "Scenes was not resized and stays unset: {out:?}");
        assert_eq!(out[1], moved[1]);
        let moved = drag(&w, 0, 10.0);
        assert_eq!(save([0.0, 0.0, 0.0], &shown, &moved, 0), [moved[0], moved[1], 0.0], "both columns beside divider 0");
        // A drag past the end of the divider's range saves Sources at its least width and gives
        // Scenes the rest of the two columns' room.
        let moved = drag(&w, 0, 500.0);
        let out = save([0.0, 0.0, 0.0], &shown, &moved, 0);
        assert!((out[1] - MIN_WIDTH).abs() < 0.01 && (out[0] + out[1] - w[0] - w[1]).abs() < 0.01, "{w:?} -> {out:?}");
        assert_eq!(out[2], 0.0);
        let out = save([250.0, 0.0, 0.0], &[0, 2], &drag(&[300.0, 979.0], 0, 15.0), 0);
        assert_eq!(out, [315.0, 0.0, 0.0], "the last column keeps no width");
        assert_eq!(save([250.0, 400.0, 0.0], &[], &[], 0), [250.0, 400.0, 0.0]);
    }

    #[test]
    fn a_rearranged_strip_gives_each_dock_its_own_width_and_the_last_the_rest() {
        assert_eq!(widths(1280.0, &[2, 0, 1], [0.0, 0.0, 0.0]), [400.0, 300.0, 578.0], "the mixer has a width once it is not last");
        assert_eq!(widths(1280.0, &[2, 0, 1], [0.0, 0.0, 450.0]), [450.0, 300.0, 528.0]);
        assert_eq!(widths(1280.0, &[1, 2, 0], [500.0, 0.0, 0.0]), [340.0, 400.0, 538.0], "Scenes is last and takes the rest");
        let shown = [2, 0, 1];
        let w = widths(1280.0, &shown, [0.0, 0.0, 0.0]);
        let out = save([0.0, 0.0, 0.0], &shown, &drag(&w, 0, 25.0), 0);
        assert_eq!(out, [275.0, 0.0, 425.0], "the mixer and Scenes, beside the first divider");
    }

    #[test]
    fn application_audio_has_a_width_of_its_own_in_the_strip() {
        let none = [0.0; crate::store::DOCK_COUNT];
        assert_eq!(widths(1280.0, &[0, 1, 2, 3], none), [300.0, 340.0, 400.0, 237.0]);
        assert_eq!(widths(1280.0, &[3, 2], none), [360.0, 919.0], "360 points by default once it is not last");
        assert_eq!(widths(1280.0, &[3, 2], [0.0, 0.0, 0.0, 420.0]), [420.0, 859.0]);
        assert_eq!(widths(1280.0, &[4, 2], none), [300.0, 979.0], "Health is 300 points by default");
        assert_eq!(widths(1280.0, &[9, 2], none), [300.0, 979.0], "an index past the last dock reads the last default");
        let shown = [3, 0, 1];
        let w = widths(1280.0, &shown, none);
        assert_eq!(save(none, &shown, &drag(&w, 0, 15.0), 0), [285.0, 0.0, 0.0, 375.0, 0.0]);
        assert_eq!(save([0.0; 3], &[9, 0], &[300.0, 900.0], 0), [0.0; 3], "an index past the saved widths is not saved");
    }

    #[test]
    fn each_divider_sits_on_its_line_and_moves_as_far_as_a_drag_can() {
        let area = Rect::from_min_size(egui::pos2(10.0, 600.0), egui::vec2(1280.0, 200.0));
        let w = widths(1280.0, &[0, 1, 2], [0.0, 0.0, 0.0]);
        let d = dividers(area, &w, 8.0);
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].at, d[0].lo, d[0].hi), (310.5, 210.5, 450.5), "Scenes 300 and Sources 340 each give way down to 200");
        assert_eq!((d[1].at, d[1].lo, d[1].hi), (651.5, 511.5, 1089.5));
        assert_eq!(d[0].bar, Rect::from_min_max(egui::pos2(306.5, 600.0), egui::pos2(314.5, 800.0)), "8 points across the line");
        assert_eq!(drag(&w, 0, d[0].hi - d[0].at), [440.0, 200.0, 638.0], "the far end of the range leaves Sources 200 wide");
        assert_eq!(drag(&w, 0, d[0].lo - d[0].at), [200.0, 440.0, 638.0]);
        let narrow = dividers(area, &[150.0, 150.0], 8.0);
        assert_eq!((narrow[0].lo, narrow[0].hi), (narrow[0].at, narrow[0].at), "columns under 200 do not give way");
        assert!(dividers(area, &[1280.0], 8.0).is_empty());
        assert!(dividers(area, &[], 8.0).is_empty());
    }

    #[test]
    fn the_top_edge_moves_between_the_least_and_the_most_height() {
        let area = Rect::from_min_size(egui::pos2(0.0, 576.0), egui::vec2(1280.0, 200.0));
        let e = edge(area, 360.0, 8.0);
        assert_eq!(e.axis, Axis::Y);
        assert_eq!((e.at, e.lo, e.hi), (576.5, 416.5, 656.5), "up to 360 and down to 120 points tall");
        assert_eq!(e.bar, Rect::from_min_max(egui::pos2(0.0, 572.5), egui::pos2(1280.0, 580.5)));
        let short = edge(Rect::from_min_size(egui::pos2(0.0, 100.0), egui::vec2(400.0, 90.0)), 360.0, 8.0);
        assert_eq!(short.hi, short.at, "a strip under 120 points does not get shorter");
        let tall = edge(area, 150.0, 8.0);
        assert_eq!(tall.lo, tall.at, "a strip over its most height does not get taller");
    }

    #[test]
    fn a_dock_window_without_a_saved_place_opens_centered_and_cascaded() {
        let bounds = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1280.0, 800.0));
        let size = egui::vec2(300.0, 200.0);
        assert_eq!(window_place(bounds, size, 0), egui::pos2(490.0, 300.0));
        assert_eq!(window_place(bounds, size, 2), egui::pos2(538.0, 348.0));
        assert_eq!(window_place(bounds, size, usize::MAX), egui::pos2(610.0, 420.0), "an index past the last dock steps no further");
        let small = Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(400.0, 300.0));
        assert_eq!(window_place(small, egui::vec2(380.0, 290.0), 3), egui::pos2(20.0, 10.0), "kept inside the main window");
        assert_eq!(window_place(small, egui::vec2(800.0, 500.0), 1), egui::Pos2::ZERO, "a larger window starts at the top left");
    }
}
