//! Working out which side of the Set up and Live switch is in force runs on every frame, so
//! comparing the docks with each built-in layout neither allocates nor frees memory. This file
//! holds one test, so no other test in the same process allocates while it counts.

use std::alloc::System;

use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};
use studiocraft_ui_egui::store::{BUILT_IN_LAYOUTS, Docks, built_in_docks};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[test]
fn comparing_the_docks_with_the_built_in_layouts_does_not_allocate() {
    let docks = [Docks::default(), Docks { order: [1, 0, 3, 2], ..Docks::default() }];
    let mut alike = 0;
    let region = Region::new(GLOBAL);
    for d in &docks {
        for name in BUILT_IN_LAYOUTS {
            if built_in_docks(name).is_some_and(|l| l.arranged_like(d)) {
                alike += 1;
            }
        }
    }
    let stats = region.change();
    assert_eq!(alike, 1, "only Classic matches the default docks");
    assert_eq!((stats.allocations, stats.reallocations, stats.deallocations), (0, 0, 0), "{stats:?}");
}
