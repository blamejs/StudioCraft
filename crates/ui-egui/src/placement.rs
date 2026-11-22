//! The main window's size and place: the first window, the smallest one, and the saved geometry
//! the window opens with.
//!
//! A first window is 1280 by 800 points and the smallest is 760 by 480. Where the window was is
//! kept in `window.json` in the configuration folder, which the UI runtime writes as the window
//! moves. Before the window opens, the saved record has its numbers repaired ([`opening`]); once
//! the window is open and the monitors are listed, a window that is off every monitor moves onto
//! one and a window larger than its monitor shrinks to it ([`on_monitors`]).

use std::path::Path;
use std::sync::{Arc, Mutex};

pub use craft_behavior::Monitor;
use craft_behavior::MonitorKey;
pub use craft_behavior::placement::Placement;
use craft_behavior::placement::{Defaults, place};
use craft_host::target::Backend;
use craft_window::restore::{FILE_NAME, MAIN, Store};

/// The size of a first window, in points.
pub const FIRST_SIZE: [f32; 2] = [1280.0, 800.0];
/// The smallest window, in points. Every dock header, the output bar and the status bar fit in it.
pub const MIN_SIZE: [f32; 2] = [760.0, 480.0];
/// The largest `window.json` that is read. A larger file is left on disk unread.
pub const MAX_FILE_BYTES: u64 = 1 << 20;

/// The first window's size and the smallest window, without positions where the platform hides
/// them (Wayland).
pub fn defaults() -> Defaults {
    Defaults::new(FIRST_SIZE, MIN_SIZE).with_no_position(Backend::from_env() == Backend::Wayland)
}

/// The store the window's placement is kept in: `window.json` in `dir`, or memory when there is no
/// folder (`--no-prefs`, scripted runs) or the file is larger than [`MAX_FILE_BYTES`].
pub fn store(dir: Option<&Path>) -> Arc<Mutex<Store>> {
    let store = match dir {
        Some(dir) => {
            let path = dir.join(FILE_NAME);
            match std::fs::metadata(&path) {
                Ok(m) if m.len() > MAX_FILE_BYTES => {
                    log::warn!("{} is {} bytes, too large to read; the window opens at its first size", path.display(), m.len());
                    Store::in_memory()
                }
                _ => Store::at(&path),
            }
        }
        None => Store::in_memory(),
    };
    Arc::new(Mutex::new(store))
}

/// The main window's saved placement in `store` with out-of-range numbers repaired, if it has one
/// whose size is a size.
pub fn saved(store: &Mutex<Store>) -> Option<Placement> {
    store.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(MAIN).and_then(|p| p.repaired())
}

/// The geometry the window opens with before the monitors are known: `saved` with a size that is
/// no size replaced by the first window's, any size raised to [`MIN_SIZE`], and full screen left
/// out. Nothing saved gives the first window, placed by the system.
pub fn opening(saved: Option<&Placement>, defaults: &Defaults) -> Placement {
    place(saved, &[], None, defaults)
}

/// `builder` opening at `placed`: its size, its position where one is kept, and maximized when it
/// was.
pub fn apply(builder: egui::ViewportBuilder, placed: &Placement, defaults: &Defaults) -> egui::ViewportBuilder {
    let mut builder = builder.with_inner_size(placed.normal.size).with_min_inner_size(defaults.min).with_maximized(placed.maximized);
    if let Some([x, y]) = placed.position() {
        builder = builder.with_position([x, y]);
    }
    builder
}

/// Where the window opened at `opened` belongs once `monitors` are listed, when that differs from
/// where it is. A window that opened from a saved record (`saved`) moves onto a monitor when its
/// title strip is on none of them, and shrinks to the usable area of its monitor when it is larger.
/// A first window shrinks to `home`, the index in `monitors` of the monitor the system opened it on,
/// and is maximized when that monitor is smaller than [`FIRST_SIZE`]. `outer_px` is the window's
/// top-left corner in desktop pixels as the system reports it; a window that is not maximized and
/// is not at its saved position (after a display's scale changed) is moved there. `None` when the
/// window stays, and when no monitor is listed.
pub fn on_monitors(
    opened: &Placement,
    saved: bool,
    monitors: &[Monitor],
    home: Option<usize>,
    outer_px: Option<[i32; 2]>,
    defaults: &Defaults,
) -> Option<Placement> {
    if monitors.is_empty() {
        return None;
    }
    let next = place(saved.then_some(opened), monitors, home, defaults);
    let now_px = if opened.maximized { opened.position_px } else { outer_px.or(opened.position_px) };
    let moved_px = next.position_px.is_some() && next.position_px != now_px;
    let moved = next.normal.size != opened.normal.size || moved_px || next.maximized != opened.maximized;
    moved.then_some(next)
}

/// The smallest size the window at `placed` may be made: [`MIN_SIZE`], lowered on an axis where
/// `placed` was fitted to a monitor smaller than that.
pub fn least_size(placed: &Placement, defaults: &Defaults) -> [f32; 2] {
    [0, 1].map(|i| defaults.min[i].min(placed.normal.size[i]).max(1.0))
}

/// The smallest size the window may be made on `monitor`: [`MIN_SIZE`], lowered on an axis where
/// the monitor's usable area is smaller than that.
pub fn least_size_on(monitor: &Monitor, defaults: &Defaults) -> [f32; 2] {
    least_size(&place(None, std::slice::from_ref(monitor), None, defaults), defaults)
}

/// A monitor the system lists: `name` (empty when it has none), its top-left corner and size in
/// pixels, and its scale.
pub fn monitor(name: Option<String>, pos_px: [i32; 2], size_px: [u32; 2], scale: f64) -> Monitor {
    Monitor::new(MonitorKey::new(name.unwrap_or_default(), size_px), pos_px, size_px, scale as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Defaults {
        Defaults::new(FIRST_SIZE, MIN_SIZE)
    }

    fn at(pos_px: [i32; 2], size: [f32; 2]) -> Placement {
        let mut p = Placement::new(size, 1.0);
        p.position_px = Some(pos_px);
        p.normal.pos = pos_px.map(|v| v as f32);
        p
    }

    fn full_hd() -> Vec<Monitor> {
        vec![monitor(Some("DP-1".into()), [0, 0], [1920, 1080], 1.0)]
    }

    #[test]
    fn saved_geometry_is_restored_or_clamped() {
        let d = defaults();
        // Nothing saved, or a record whose size is no size: the first window, placed by the system.
        for saved in [None, Some(Placement::new([0.0, 0.0], 1.0)), Some(Placement::new([f32::NAN, 600.0], 1.0))] {
            let first = opening(saved.as_ref(), &d);
            assert_eq!((first.normal.size, first.position_px, first.maximized), (FIRST_SIZE, None, false), "{saved:?}");
        }
        // A window on the monitor that fits it opens where it was, at its size.
        let kept = at([100, 80], [1000.0, 700.0]);
        let opened = opening(Some(&kept), &d);
        assert_eq!((opened.normal.size, opened.position_px), ([1000.0, 700.0], Some([100, 80])));
        assert_eq!(on_monitors(&opened, true, &full_hd(), None, Some([100, 80]), &d), None);
        // The system put it elsewhere (the display's scale changed since): it moves back.
        let back = on_monitors(&opened, true, &full_hd(), None, Some([150, 120]), &d).and_then(|p| p.position_px);
        assert_eq!(back, Some([100, 80]));
        // The first window on a monitor with room for it stays as the system placed it.
        assert_eq!(on_monitors(&opening(None, &d), false, &full_hd(), None, Some([300, 140]), &d), None);
        // A size below the smallest window is raised to it.
        assert_eq!(opening(Some(&at([0, 0], [300.0, 200.0])), &d).normal.size, MIN_SIZE);
        // A maximized window opens maximized, keeping its normal size for later.
        let mut maximized = kept.clone();
        maximized.maximized = true;
        assert!(opening(Some(&maximized), &d).maximized);
        // Full screen is not restored.
        let mut full = kept.clone();
        full.fullscreen = true;
        assert!(!opening(Some(&full), &d).fullscreen);
    }

    #[test]
    fn a_window_off_every_monitor_moves_onto_one() {
        let d = defaults();
        // Saved on a monitor to the right that is no longer connected.
        let gone = opening(Some(&at([2500, 300], [1000.0, 700.0])), &d);
        let Some(moved) = on_monitors(&gone, true, &full_hd(), None, Some([2500, 300]), &d) else { panic!("the window stays off screen") };
        let [x, y] = moved.position_px.unwrap_or([i32::MIN; 2]);
        assert!((0..=1920 - 1000).contains(&x) && (0..=1080 - 700).contains(&y), "{moved:?}");
        assert_eq!(moved.normal.size, [1000.0, 700.0]);
        // Far above and to the left of every monitor, as a hostile file may say.
        let lost = opening(Some(&at([i32::MIN, i32::MIN], [1000.0, 700.0])), &d);
        let moved = on_monitors(&lost, true, &full_hd(), None, None, &d).and_then(|p| p.position_px);
        assert!(moved.is_some_and(|[x, y]| x >= 0 && y >= 0), "{moved:?}");
        // On a second monitor that is still connected, the window stays.
        let two = vec![monitor(None, [0, 0], [1920, 1080], 1.0), monitor(None, [1920, 0], [2560, 1440], 1.0)];
        assert_eq!(on_monitors(&gone, true, &two, Some(1), Some([2500, 300]), &d), None);
        // Without a monitor list nothing is checked.
        assert_eq!(on_monitors(&gone, true, &[], None, None, &d), None);
    }

    #[test]
    fn a_window_larger_than_its_monitor_shrinks_to_it() {
        let d = defaults();
        let big = opening(Some(&at([0, 0], [3000.0, 2000.0])), &d);
        let Some(fitted) = on_monitors(&big, true, &full_hd(), None, Some([0, 0]), &d) else { panic!("the window stays larger than the monitor") };
        let [w, h] = fitted.normal.size;
        assert!(w <= 1920.0 && h <= 1080.0 && w >= MIN_SIZE[0] && h >= MIN_SIZE[1], "{fitted:?}");
        assert_eq!(least_size(&fitted, &d), MIN_SIZE);
        // The first window on a monitor smaller than it shrinks too, never below the smallest
        // window while the monitor has room for that, and is maximized.
        let first = opening(None, &d);
        let small = vec![monitor(None, [0, 0], [1024, 600], 1.0)];
        let Some(fitted) = on_monitors(&first, false, &small, None, None, &d) else { panic!("the first window stays larger than the monitor") };
        let [w, h] = fitted.normal.size;
        assert!(w <= 1024.0 && h <= 600.0 && w >= MIN_SIZE[0] && h >= MIN_SIZE[1], "{fitted:?}");
        assert!(fitted.maximized, "{fitted:?}");
        // The system opened the first window on a small second monitor: it is sized to that one,
        // and on the large one it stays.
        let two = vec![monitor(None, [0, 0], [1920, 1080], 1.0), monitor(None, [1920, 0], [1024, 600], 1.0)];
        let fitted = on_monitors(&first, false, &two, Some(1), None, &d);
        assert!(fitted.as_ref().is_some_and(|p| p.maximized && p.normal.size[0] <= 1024.0), "{fitted:?}");
        assert_eq!(on_monitors(&first, false, &two, Some(0), None, &d), None);
        // On a monitor smaller than the smallest window, the window fits it and may be that small.
        let tiny = vec![monitor(None, [0, 0], [640, 400], 1.0)];
        let Some(fitted) = on_monitors(&first, false, &tiny, None, None, &d) else { panic!("the first window stays larger than the monitor") };
        let [w, h] = fitted.normal.size;
        assert!(w <= 640.0 && h <= 400.0, "{fitted:?}");
        assert_eq!(least_size(&fitted, &d), [MIN_SIZE[0].min(w), MIN_SIZE[1].min(h)]);
        assert!(least_size(&fitted, &d).iter().zip(MIN_SIZE).any(|(l, m)| *l < m), "{fitted:?}");
        // Moved on to a monitor with room for the smallest window, the window gets that limit back.
        assert_eq!(least_size_on(&tiny[0], &d), least_size(&fitted, &d));
        assert_eq!(least_size_on(&full_hd()[0], &d), MIN_SIZE);
    }

    #[test]
    fn the_placement_file_is_kept_in_the_folder_and_a_huge_one_is_not_read() {
        let dir = std::env::temp_dir().join(format!("studiocraft-placement-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let kept = at([40, 30], [900.0, 600.0]);
        {
            let store = store(Some(&dir));
            let mut s = store.lock().unwrap();
            s.set(MAIN, &kept);
            assert_eq!(s.save().map_err(|e| e.to_string()), Ok(true));
        }
        assert_eq!(saved(&store(Some(&dir))), Some(kept));
        std::fs::write(dir.join(FILE_NAME), vec![b' '; MAX_FILE_BYTES as usize + 1]).unwrap();
        let huge = store(Some(&dir));
        assert_eq!(saved(&huge), None);
        assert_eq!(huge.lock().unwrap().path(), None, "an oversized file is neither read nor written");
        assert_eq!(saved(&store(None)), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
