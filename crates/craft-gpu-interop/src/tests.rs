use std::time::{Duration, Instant};

use super::*;

fn frame(ring: &std::sync::Arc<SharedRing>, slot: usize, ready: u64) -> SharedFrame {
    SharedFrame::new(std::sync::Arc::clone(ring), slot, ready).unwrap()
}

#[test]
fn free_textures_are_written_oldest_first() {
    let t0 = Instant::now();
    let ring = SharedRing::detached(1, 1);
    let mut slots = SlotTable::default();
    let mut order = Vec::new();
    for _ in 0..SLOTS {
        let s = slots.pick(&ring, 0, t0).unwrap();
        assert!(!order.contains(&s), "a texture handed out is not picked again");
        order.push(s);
        slots.fill(&ring, s, t0).unwrap();
    }
    assert_eq!(slots.last(), 3);
    assert_eq!(slots.pick(&ring, 0, t0), None, "every texture is waiting for the compositor");
    // The compositor read frame 1: its texture is the one written again.
    assert_eq!(slots.pick(&ring, 1, t0), Some(order[0]));
    // Frame 3 read means frames 1 and 2 were passed over; all are free, the oldest first.
    assert_eq!(slots.pick(&ring, 3, t0), Some(order[0]));
}

#[test]
fn a_texture_the_compositor_never_releases_is_taken_back() {
    let t0 = Instant::now();
    let ring = SharedRing::detached(1, 1);
    let mut slots = SlotTable::default();
    for _ in 0..SLOTS {
        let s = slots.pick(&ring, 0, t0).unwrap();
        slots.fill(&ring, s, t0).unwrap();
    }
    assert_eq!(slots.pick(&ring, 0, t0 + RECLAIM - Duration::from_millis(1)), None);
    let s = slots.pick(&ring, 0, t0 + RECLAIM).unwrap();
    assert_eq!(slots.fill(&ring, s, t0 + RECLAIM), Some(4));
    // A clock that went back does not free a texture.
    let later = t0 + RECLAIM * 4;
    let mut slots = SlotTable::default();
    for _ in 0..SLOTS {
        let s = slots.pick(&ring, 0, later).unwrap();
        slots.fill(&ring, s, later).unwrap();
    }
    assert_eq!(slots.pick(&ring, 0, t0), None);
}

#[test]
fn a_texture_the_compositor_started_to_read_waits_for_the_released_fence() {
    let t0 = Instant::now();
    let ring = SharedRing::detached(1, 1);
    let mut slots = SlotTable::default();
    let mut frames = Vec::new();
    for _ in 0..SLOTS {
        let s = slots.pick(&ring, 0, t0).unwrap();
        frames.push(frame(&ring, s, slots.fill(&ring, s, t0).unwrap()));
    }
    // The compositor submits a draw of frame 1 that is still running after RECLAIM.
    assert!(frames[0].start_reading());
    assert!(!frames[0].start_reading(), "a frame is read once");
    let late = t0 + RECLAIM * 2;
    let s = slots.pick(&ring, 0, late).unwrap();
    assert_ne!(s, frames[0].slot(), "the texture being read is not written");
    assert_eq!(s, frames[1].slot(), "the oldest texture nobody reads is taken back");
    // A frame taken back is not drawn: its texture may hold another picture.
    assert!(!frames[1].start_reading());
    assert!(!frames[2].start_reading(), "a texture past RECLAIM that was passed over is taken back too");
    slots.fill(&ring, s, late).unwrap();
    assert_eq!(slots.pick(&ring, 0, late), Some(frames[2].slot()));
    // Once the released fence passes frame 1, its texture is free again.
    assert_eq!(slots.pick(&ring, 1, late), Some(frames[0].slot()));
}

#[test]
fn an_export_texture_is_written_again_only_once_it_is_released() {
    let t0 = Instant::now();
    let ring = SharedRing::detached(4, 2);
    let mut slots = SlotTable::default();
    for _ in 0..SLOTS {
        let s = slots.pick_released(0).unwrap();
        slots.fill_released(s, t0).unwrap();
    }
    assert_eq!(slots.pick_released(0), None, "every texture is being encoded");
    // However long the encoder holds them, the textures are not taken back: they were never
    // handed out as shared frames, so the capture side's time limit does not apply.
    assert_eq!(slots.pick(&ring, 0, t0 + RECLAIM * 10), None);
    assert_eq!(slots.pick_released(0), None);
    assert_eq!(slots.pick_released(2), slots.pick(&ring, 2, t0), "frames 1 and 2 were read; the oldest goes first");
}

#[test]
fn a_slot_outside_the_ring_is_refused() {
    let ring = SharedRing::detached(4, 2);
    let mut slots = SlotTable::default();
    assert_eq!(slots.fill(&ring, SLOTS, Instant::now()), None);
    assert_eq!(slots.last(), 0, "a refused fill hands out no value");
    assert!(SharedFrame::new(ring.clone(), SLOTS, 1).is_none());
    assert!(SharedFrame::new(ring.clone(), usize::MAX, 1).is_none());
    let f = frame(&ring, SLOTS - 1, 9);
    assert_eq!((f.slot(), f.ready(), f.width(), f.height()), (SLOTS - 1, 9, 4, 2));
}

#[test]
fn a_late_copy_leaves_the_frame_waiting_and_the_caller_goes_on() {
    let ring = SharedRing::detached(1, 1);
    let mut handoff = Handoff::default();
    assert!(handoff.take_ready(u64::MAX).is_none(), "nothing offered");
    handoff.offer(frame(&ring, 0, 5));
    // The capture side's fence is behind: each call returns at once and the frame stays.
    for completed in [0, 3, 4] {
        assert!(handoff.take_ready(completed).is_none());
        assert_eq!(handoff.pending().map(SharedFrame::ready), Some(5));
    }
    // A newer frame takes the waiting one's place.
    handoff.offer(frame(&ring, 1, 6));
    assert!(handoff.take_ready(4).is_none());
    assert_eq!(handoff.pending().map(SharedFrame::ready), Some(6));
    let taken = handoff.take_ready(6).unwrap();
    assert_eq!((taken.slot(), taken.ready()), (1, 6));
    assert!(handoff.pending().is_none(), "a frame is taken once");
    assert!(handoff.take_ready(u64::MAX).is_none(), "the frame it replaced is dropped with it");
}

#[test]
fn a_finished_frame_is_drawn_while_the_newer_one_is_copied() {
    let ring = SharedRing::detached(1, 1);
    let mut handoff = Handoff::default();
    handoff.offer(frame(&ring, 0, 5));
    handoff.offer(frame(&ring, 1, 6));
    // Frame 5's copy is done and frame 6's is not: frame 5 is taken and frame 6 keeps waiting.
    assert_eq!(handoff.take_ready(5).map(|f| f.ready()), Some(5));
    assert_eq!(handoff.pending().map(SharedFrame::ready), Some(6));
    assert!(handoff.take_ready(5).is_none(), "the older frame is taken once");
    // Only the frame the newest one replaced is kept: frame 6 is dropped.
    handoff.offer(frame(&ring, 2, 7));
    handoff.offer(frame(&ring, 0, 8));
    assert!(handoff.take_ready(6).is_none());
    assert_eq!(handoff.take_ready(7).map(|f| f.ready()), Some(7));
    // A frame from another ring is not kept.
    let other = SharedRing::detached(1, 1);
    handoff.offer(frame(&other, 0, 9));
    assert!(handoff.take_ready(8).is_none());
    assert_eq!(handoff.take_ready(9).map(|f| f.ready()), Some(9));
}

#[test]
fn the_first_fallback_reason_is_kept() {
    let ring = SharedRing::detached(1, 1);
    assert_eq!(ring.fallback(), None);
    ring.refuse(Fallback::UnsupportedBackend);
    ring.refuse(Fallback::OtherAdapter);
    assert_eq!(ring.fallback(), Some(Fallback::UnsupportedBackend));
    for f in Fallback::ALL {
        assert_eq!(Fallback::from_code(f.code()), Some(f));
        assert!(!f.to_string().is_empty());
    }
    assert_eq!(Fallback::from_code(0), None);
    assert_eq!(Fallback::from_code(200), None);
    assert_ne!(SharedRing::detached(1, 1).id(), ring.id(), "every ring has its own id");
}

#[test]
fn an_import_needs_direct3d_12_on_the_display_adapter() {
    let a = AdapterId::from_parts(7, 0);
    let b = AdapterId::from_parts(7, 1);
    assert_eq!(check_import(Backend::Dx12, a, Some(a)), Ok(()));
    assert_eq!(check_import(Backend::Other, a, Some(a)), Err(Fallback::UnsupportedBackend));
    assert_eq!(check_import(Backend::Dx12, a, Some(b)), Err(Fallback::OtherAdapter));
    assert_eq!(check_import(Backend::Dx12, a, None), Err(Fallback::OtherAdapter));
}

#[test]
fn a_vulkan_import_needs_the_display_adapter_the_external_memory_extension_and_timeline_semaphores() {
    let a = AdapterId::from_parts(7, 0);
    let b = AdapterId::from_parts(8, 0);
    let with = Backend::Vulkan { external_memory: true, timeline: true };
    let without = Backend::Vulkan { external_memory: false, timeline: true };
    let no_timeline = Backend::Vulkan { external_memory: true, timeline: false };
    assert_eq!(check_import(with, a, Some(a)), Ok(()), "a Vulkan compositor on the display's adapter opens the ring");
    assert_eq!(check_import(without, a, Some(a)), Err(Fallback::MissingExtension));
    assert_eq!(check_import(no_timeline, a, Some(a)), Err(Fallback::MissingExtension));
    assert_eq!(check_import(Backend::Vulkan { external_memory: false, timeline: false }, a, Some(a)), Err(Fallback::MissingExtension));
    // The adapter is checked before the extension: either way the ring cannot be opened.
    assert_eq!(check_import(with, a, Some(b)), Err(Fallback::OtherAdapter));
    assert_eq!(check_import(without, a, Some(b)), Err(Fallback::OtherAdapter));
    // A driver that does not report its adapter counts as another adapter.
    assert_eq!(check_import(with, a, None), Err(Fallback::OtherAdapter));
    assert_eq!(check_import(Backend::Other, a, None), Err(Fallback::UnsupportedBackend));
}

#[test]
fn a_failed_open_destroys_the_resources_opened_before_it() {
    let mut destroyed = Vec::new();
    let result = crate::fallback::open_all(&[1, 2, 3], |h| if h == 3 { Err(Fallback::ImportFailed) } else { Ok(h * 10) }, |r| destroyed.push(r));
    assert_eq!(result, Err(Fallback::ImportFailed));
    assert_eq!(destroyed, [10, 20], "both textures opened before the failure are destroyed");

    let mut destroyed = Vec::new();
    let result = crate::fallback::open_all(&[1, 2, 3], |h| Ok::<_, Fallback>(h * 10), |r| destroyed.push(r));
    assert_eq!(result, Ok(vec![10, 20, 30]));
    assert!(destroyed.is_empty(), "nothing is destroyed when every open succeeds");

    let result = crate::fallback::open_all(&[1], |_| Err::<u32, _>(Fallback::ImportFailed), |_| panic!("nothing was opened"));
    assert_eq!(result, Err(Fallback::ImportFailed));
}

#[test]
fn an_adapter_id_keeps_both_parts() {
    assert_eq!(AdapterId::from_parts(1, 2).0, (2 << 32) | 1);
    assert_eq!(AdapterId::from_parts(u32::MAX, -1).0, u64::MAX);
    assert_ne!(AdapterId::from_parts(0, -1), AdapterId::from_parts(0, 1));
    for (low, high) in [(0, 0), (1, 2), (u32::MAX, -1), (0x1234_5678, i32::MIN)] {
        assert_eq!(AdapterId::from_parts(low, high).parts(), (low, high));
    }
}

#[test]
fn an_adapter_id_from_vulkan_is_the_one_windows_reports() {
    // Vulkan's deviceLUID is the Windows LUID's bytes: the low part, then the high part.
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&0x0000_d2a1u32.to_le_bytes());
    bytes[4..].copy_from_slice(&(-2i32).to_le_bytes());
    assert_eq!(AdapterId::from_luid_bytes(bytes), AdapterId::from_parts(0x0000_d2a1, -2));
    assert_eq!(AdapterId::from_luid_bytes([0; 8]), AdapterId(0));
}

#[cfg(not(windows))]
#[test]
fn elsewhere_than_windows_the_capture_reports_the_fallback() {
    assert_eq!(Duplication::open("display").err(), Some(CaptureError::Fallback(Fallback::Unsupported)));
    assert_eq!(WindowCapture::open(1, "display").err(), Some(CaptureError::Fallback(Fallback::Unsupported)));
}

mod window {
    use std::collections::VecDeque;

    use super::*;
    use crate::window_capture::{Capture, MAX_SIDE, Plan, Step, Target, open_error, plan, sides};

    struct Picture {
        content: (i32, i32),
        size: (u32, u32),
    }

    fn picture(w: u32, h: u32) -> Picture {
        Picture { content: (w as i32, h as i32), size: (w, h) }
    }

    /// The system side of a window capture, without a system: pictures are queued by the test.
    struct Fake {
        alive: bool,
        pictures: VecDeque<Picture>,
        pool: (i32, i32),
        released: u64,
        rings: Vec<(u32, u32)>,
        resized: Vec<(i32, i32)>,
        copies: Vec<(usize, u64, (u32, u32))>,
    }

    impl Fake {
        fn new(w: i32, h: i32) -> Fake {
            Fake { alive: true, pictures: VecDeque::new(), pool: (w, h), released: 0, rings: Vec::new(), resized: Vec::new(), copies: Vec::new() }
        }
    }

    impl Target for Fake {
        type Picture = Picture;
        fn alive(&self) -> bool {
            self.alive
        }
        fn next(&mut self) -> Result<Option<Picture>, CaptureError> {
            // The newest picture wins, as the system's frame pool hands out the newest one.
            let newest = self.pictures.pop_back();
            self.pictures.clear();
            Ok(newest)
        }
        fn sizes(p: &Picture) -> ((i32, i32), (u32, u32)) {
            (p.content, p.size)
        }
        fn pool_size(&self) -> (i32, i32) {
            self.pool
        }
        fn resize_pool(&mut self, width: i32, height: i32) -> Result<(), CaptureError> {
            self.pool = (width, height);
            self.resized.push((width, height));
            Ok(())
        }
        fn new_ring(&mut self, width: u32, height: u32) -> Result<std::sync::Arc<SharedRing>, CaptureError> {
            self.rings.push((width, height));
            Ok(SharedRing::detached(width, height))
        }
        fn released(&self) -> u64 {
            self.released
        }
        fn copy(&mut self, p: Picture, slot: usize, ready: u64) -> Result<(), CaptureError> {
            self.copies.push((slot, ready, p.size));
            Ok(())
        }
    }

    fn shared(g: Result<Grab, CaptureError>) -> SharedFrame {
        match g {
            Ok(Grab::Frame(f)) => f,
            other => panic!("expected a frame, got {other:?}"),
        }
    }

    #[test]
    fn an_older_windows_or_a_refused_window_falls_back_and_a_closed_one_is_looked_for_again() {
        const E_NOINTERFACE: i32 = 0x8000_4002_u32 as i32;
        const REGDB_E_CLASSNOTREG: i32 = 0x8004_0154_u32 as i32;
        const E_ACCESSDENIED: i32 = 0x8007_0005_u32 as i32;
        const E_INVALIDARG: i32 = 0x8007_0057_u32 as i32;
        const INVALID_WINDOW: i32 = 0x8007_0578_u32 as i32;
        const DEVICE_REMOVED: i32 = 0x887A_0005_u32 as i32;
        let unsupported = CaptureError::Fallback(Fallback::Unsupported);
        let refused = CaptureError::Fallback(Fallback::Refused);
        // An older Windows: no capture classes, or a pool or session interface it does not have.
        assert_eq!(open_error(Step::Support, REGDB_E_CLASSNOTREG, ""), unsupported);
        assert_eq!(open_error(Step::Support, E_INVALIDARG, ""), unsupported);
        assert_eq!(open_error(Step::Item, REGDB_E_CLASSNOTREG, ""), unsupported);
        assert_eq!(open_error(Step::Pool, E_NOINTERFACE, ""), unsupported);
        // A window the system does not let be captured.
        assert_eq!(open_error(Step::Item, E_ACCESSDENIED, ""), refused);
        assert_eq!(open_error(Step::Item, E_INVALIDARG, ""), refused);
        assert_eq!(open_error(Step::Session, E_ACCESSDENIED, ""), refused);
        // A window that closed before it was opened, and a device that went away.
        assert!(matches!(open_error(Step::Item, INVALID_WINDOW, "gone"), CaptureError::Lost(m) if m.contains("gone")));
        assert!(matches!(open_error(Step::Pool, DEVICE_REMOVED, ""), CaptureError::Failed(_)));
        assert!(matches!(open_error(Step::Device, E_INVALIDARG, ""), CaptureError::Failed(_)));
    }

    #[test]
    fn a_window_ring_on_another_adapter_or_that_failed_to_import_sends_the_capture_back_to_memory() {
        for reason in [Fallback::OtherAdapter, Fallback::ImportFailed, Fallback::UnsupportedBackend] {
            let mut capture = Capture::new(Fake::new(4, 2)).unwrap();
            capture.ring().refuse(reason);
            assert_eq!(capture.grab(Instant::now()).err(), Some(CaptureError::Fallback(reason)));
        }
        // The importer's adapter check is the one the display capture uses.
        let window = AdapterId::from_parts(3, 0);
        assert_eq!(check_import(Backend::Dx12, window, Some(AdapterId::from_parts(4, 0))), Err(Fallback::OtherAdapter));
    }

    #[test]
    fn a_window_picture_is_copied_into_the_ring_and_a_closed_window_is_lost() {
        let mut capture = Capture::new(Fake::new(4, 2)).unwrap();
        let now = Instant::now();
        assert!(matches!(capture.grab(now), Ok(Grab::Unchanged)), "no picture yet");
        capture.target.pictures.push_back(picture(4, 2));
        let f = shared(capture.grab(now));
        assert_eq!((f.slot(), f.ready(), f.width(), f.height()), (0, 1, 4, 2));
        assert_eq!(capture.target.copies, vec![(0, 1, (4, 2))]);
        assert!(matches!(capture.grab(now), Ok(Grab::Unchanged)), "an unchanged window gives nothing");
        capture.target.alive = false;
        assert!(matches!(capture.grab(now), Err(CaptureError::Lost(_))));
    }

    #[test]
    fn a_resized_window_gets_buffers_and_then_a_ring_of_its_new_size() {
        let mut capture = Capture::new(Fake::new(4, 2)).unwrap();
        let now = Instant::now();
        let first = capture.ring().id();
        // The window grew: the picture in the old buffer holds only part of it and is dropped.
        capture.target.pictures.push_back(Picture { content: (8, 6), size: (4, 2) });
        assert!(matches!(capture.grab(now), Ok(Grab::Unchanged)));
        assert_eq!(capture.target.resized, vec![(8, 6)]);
        assert!(capture.target.copies.is_empty());
        // The next picture is in a buffer of the new size and goes into a new ring of that size.
        capture.target.pictures.push_back(picture(8, 6));
        let f = shared(capture.grab(now));
        assert_eq!((f.width(), f.height(), f.ready()), (8, 6, 1));
        assert_ne!(f.ring().id(), first);
        assert_eq!(capture.target.rings, vec![(4, 2), (8, 6)]);
    }

    #[test]
    fn a_minimized_or_oversized_window_gives_no_picture() {
        let mut capture = Capture::new(Fake::new(4, 2)).unwrap();
        let now = Instant::now();
        capture.target.pictures.push_back(Picture { content: (0, 0), size: (4, 2) });
        assert!(matches!(capture.grab(now), Ok(Grab::Unchanged)));
        assert!(capture.target.resized.is_empty() && capture.target.copies.is_empty(), "a minimized window keeps its buffers");
        let big = MAX_SIDE as i32 + 1;
        capture.target.pictures.push_back(Picture { content: (big, 2), size: (4, 2) });
        assert!(matches!(capture.grab(now), Err(CaptureError::Failed(_))), "no shared texture holds it");
        assert!(matches!(sides(0, 5), Err(CaptureError::Lost(_))));
        assert!(matches!(sides(-1, 5), Err(CaptureError::Lost(_))));
        assert_eq!(sides(MAX_SIDE as i32, 1).ok(), Some((MAX_SIDE, 1)));
        assert!(Capture::new(Fake::new(0, 0)).is_err(), "a window with no area opens no ring");
        assert_eq!(plan((4, 2), (4, 2), (4, 2), (4, 2)).ok(), Some(Plan::Copy { new_ring: false }));
        assert_eq!(plan((4, 2), (0, 2), (4, 2), (4, 2)).ok(), Some(Plan::Skip));
    }

    #[test]
    fn a_late_release_leaves_the_newest_picture_held_and_the_capture_goes_on() {
        let mut capture = Capture::new(Fake::new(4, 2)).unwrap();
        let now = Instant::now();
        for ready in 1..=SLOTS as u64 {
            capture.target.pictures.push_back(picture(4, 2));
            assert_eq!(shared(capture.grab(now)).ready(), ready);
        }
        // The compositor has released none of them: the capture returns at once and keeps the
        // newest picture, and a newer one replaces it.
        capture.target.pictures.push_back(picture(4, 2));
        assert!(matches!(capture.grab(now), Ok(Grab::Unchanged)));
        capture.target.pictures.push_back(Picture { content: (4, 2), size: (4, 2) });
        assert!(matches!(capture.grab(now), Ok(Grab::Unchanged)));
        assert_eq!(capture.target.copies.len(), SLOTS);
        // Once the first texture is released, the held picture is copied without a new one.
        capture.target.released = 1;
        let f = shared(capture.grab(now));
        assert_eq!((f.slot(), f.ready()), (0, SLOTS as u64 + 1));
        assert!(matches!(capture.grab(now), Ok(Grab::Unchanged)), "the held picture is copied once");
    }
}

fn exported(ring: &std::sync::Arc<ExportRing>, slot: usize, ready: u64) -> ExportFrame {
    ExportFrame::new(std::sync::Arc::clone(ring), slot, ready).unwrap()
}

#[test]
fn an_export_needs_a_known_compositor_adapter_and_a_backend_that_shares_textures() {
    let a = AdapterId::from_parts(7, 0);
    let with = Backend::Vulkan { external_memory: true, timeline: true };
    assert_eq!(check_export(Backend::Dx12, Some(a)), Ok(a), "the ring is made on the compositor's own adapter");
    assert_eq!(check_export(with, Some(a)), Ok(a));
    assert_eq!(check_export(Backend::Other, Some(a)), Err(Fallback::UnsupportedBackend));
    assert_eq!(check_export(Backend::Other, None), Err(Fallback::UnsupportedBackend));
    assert_eq!(check_export(Backend::Dx12, None), Err(Fallback::OtherAdapter), "a driver that does not report its adapter");
    assert_eq!(check_export(with, None), Err(Fallback::OtherAdapter));
    assert_eq!(check_export(Backend::Vulkan { external_memory: false, timeline: true }, Some(a)), Err(Fallback::MissingExtension));
    assert_eq!(check_export(Backend::Vulkan { external_memory: true, timeline: false }, Some(a)), Err(Fallback::MissingExtension));
}

#[test]
fn an_export_ring_needs_even_sides() {
    let a = AdapterId::from_parts(7, 0);
    for (w, h) in [(0, 2), (2, 0), (3, 2), (2, 3), (u32::MAX, 2)] {
        assert_eq!(ExportRing::open(a, w, h).err(), Some(Fallback::ImportFailed), "{w} x {h}");
    }
    #[cfg(not(windows))]
    assert_eq!(ExportRing::open(a, 64, 2).err(), Some(Fallback::Unsupported));
}

#[test]
fn the_encoders_refusal_is_kept_and_frames_name_a_slot_of_the_ring() {
    let ring = ExportRing::detached(4, 2);
    assert_eq!(ring.fallback(), None);
    ring.refuse(Fallback::NoEncoder);
    ring.refuse(Fallback::ImportFailed);
    assert_eq!(ring.fallback(), Some(Fallback::NoEncoder));
    assert_eq!(Fallback::from_code(Fallback::NoEncoder.code()), Some(Fallback::NoEncoder));
    assert!(ExportFrame::new(ring.clone(), SLOTS, 1).is_none());
    assert!(ExportFrame::new(ring.clone(), usize::MAX, 1).is_none());
    let f = exported(&ring, SLOTS - 1, 9);
    assert_eq!((f.slot(), f.ready(), f.width(), f.height()), (SLOTS - 1, 9, 4, 2));
    assert_ne!(ExportRing::detached(4, 2).id(), ring.id(), "every ring has its own id");
}

#[test]
fn a_late_copy_leaves_the_frame_queued_and_the_encoder_goes_on() {
    let ring = ExportRing::detached(64, 2);
    let mut feed = Feed::default();
    assert!(feed.take_ready().is_none(), "nothing offered");
    assert!(!feed.copy_running(), "nothing waits for a copy");
    assert!(feed.offer(exported(&ring, 0, 1), 100).is_none());
    assert!(feed.offer(exported(&ring, 1, 2), 200).is_none());
    // The compositor's copy is behind: each call returns at once and both frames stay queued.
    for _ in 0..3 {
        assert!(feed.take_ready().is_none());
        assert_eq!((feed.waiting(), feed.reading()), (2, 0));
        assert!(feed.copy_running(), "a flush waits for this copy");
    }
    // The first copy finished: only that frame is taken, in order.
    ring.set_ready(1);
    assert!(!feed.copy_running(), "the oldest frame can be encoded");
    let (first, pts) = feed.take_ready().unwrap();
    assert_eq!((first.ready(), pts), (1, 100));
    assert!(feed.take_ready().is_none(), "the second copy is still running");
    assert!(feed.copy_running());
    assert_eq!((feed.waiting(), feed.reading()), (1, 1));
    // The texture goes back only once the frame's packet is out.
    assert_eq!(feed.out(0), 0);
    assert_eq!(ring.released(), 0);
    assert_eq!(feed.out(1), 1);
    assert_eq!(ring.released(), 1);
    ring.set_ready(2);
    assert_eq!(feed.take_ready().map(|(f, p)| (f.ready(), p)), Some((2, 200)));
    assert_eq!(feed.out(5), 1, "only frames being read are handed back");
    assert_eq!(ring.released(), 2);
    // A lower value does not take the released fence back.
    ring.release(1);
    assert_eq!(ring.released(), 2);
}

#[test]
fn a_frame_beyond_the_rings_textures_drops_the_oldest_and_a_stopped_encoder_hands_back_finished_copies() {
    let ring = ExportRing::detached(64, 2);
    let mut feed = Feed::default();
    for value in 1..=SLOTS as u64 {
        assert!(feed.offer(exported(&ring, value as usize - 1, value), value as i64).is_none());
    }
    let dropped = feed.offer(exported(&ring, 0, SLOTS as u64 + 1), SLOTS as i64 + 1).unwrap();
    assert_eq!(dropped.ready(), 1, "the oldest frame, whose texture was written again");
    assert_eq!(ring.released(), 0, "a dropped frame does not hand back the frames before it");
    assert_eq!(feed.waiting(), SLOTS);
    ring.set_ready(2);
    assert!(feed.take_ready().is_some());
    // A frame being encoded is not handed back by a later frame that is dropped.
    assert!(feed.offer(exported(&ring, 1, SLOTS as u64 + 2), SLOTS as i64 + 2).is_none());
    assert_eq!(feed.offer(exported(&ring, 2, SLOTS as u64 + 3), SLOTS as i64 + 3).map(|f| f.ready()), Some(3));
    assert_eq!(ring.released(), 0);
    assert_eq!(feed.reading(), 1);
    assert!(!feed.stalled());
    // The copy of the newest frame is still running when the encoder stops.
    ring.set_ready(SLOTS as u64 + 2);
    assert_eq!(feed.drain(), SLOTS, "the frames not taken are not encoded");
    assert_eq!((feed.waiting(), feed.reading()), (0, 0));
    assert_eq!(ring.released(), SLOTS as u64 + 2, "every texture but the one still being written is handed back");
    assert_eq!(feed.drain(), 0);
    assert_eq!(ring.released(), SLOTS as u64 + 2);
}

#[test]
fn an_encoder_that_holds_every_texture_with_no_packet_out_is_stalled() {
    let ring = ExportRing::detached(64, 2);
    let mut feed = Feed::default();
    ring.set_ready(u64::MAX);
    for value in 1..=SLOTS as u64 {
        assert!(!feed.stalled(), "a texture is still free");
        assert!(feed.offer(exported(&ring, value as usize - 1, value), value as i64).is_none());
        assert!(feed.take_ready().is_some());
    }
    assert!(feed.stalled(), "the compositor has no texture left to export into");
    assert_eq!(feed.out(1), 1);
    assert!(!feed.stalled(), "a packet out frees a texture");
}

#[test]
fn frames_the_queue_dropped_still_hold_their_textures_until_a_later_release() {
    let ring = ExportRing::detached(64, 2);
    let mut feed = Feed::default();
    // The queue to the encoder dropped frame 1; frames 2 and 3 arrive and are being read.
    ring.set_ready(3);
    for value in 2..=3 {
        assert!(feed.offer(exported(&ring, value as usize - 1, value), value as i64).is_none());
        assert!(feed.take_ready().is_some());
    }
    assert!(feed.stalled(), "frame 1's texture is not handed back yet either");
    assert_eq!(feed.out(1), 1);
    assert!(!feed.stalled(), "frame 2's release covers frame 1");
    // A frame still to be encoded is not a stall.
    let mut feed = Feed::default();
    for value in 4..=6 {
        assert!(feed.offer(exported(&ring, value as usize - 4, value), value as i64).is_none());
    }
    ring.set_ready(5);
    assert!(feed.take_ready().is_some() && feed.take_ready().is_some());
    assert!(!feed.stalled(), "frame 6 still waits for its copy");
}

#[test]
fn a_session_on_a_ring_read_before_counts_only_its_own_frames() {
    let ring = ExportRing::detached(64, 2);
    ring.set_ready(10);
    ring.release(8);
    let mut feed = Feed::default();
    for value in 9..=10 {
        assert!(feed.offer(exported(&ring, value as usize - 9, value), value as i64).is_none());
        assert!(feed.take_ready().is_some());
    }
    assert!(!feed.stalled(), "one texture is still free");
}

#[test]
fn a_release_while_another_is_signaling_returns_at_once_and_its_value_is_signaled() {
    let gate = export::ReleaseGate::default();
    let mut signaled = Vec::new();
    let held = gate.release(5, |value| {
        if signaled.is_empty() {
            // A second thread releases while this call holds the gate; it must not wait for it.
            let other = std::thread::scope(|s| s.spawn(|| gate.release(9, |_| panic!("only the caller that holds the gate signals"))).join());
            assert!(matches!(other, Ok(true)), "the second release returns without signaling");
        }
        signaled.push(value);
        true
    });
    assert!(held);
    assert_eq!(signaled, [5, 9], "the holder signals the value left while it was signaling");
    // A value not above the highest one signaled is not signaled again.
    assert!(gate.release(7, |_| panic!("the fence never goes back")));
    // A signal that cannot be queued is reported, and the next release tries the value again.
    assert!(!gate.release(12, |_| false));
    let mut again = Vec::new();
    assert!(gate.release(10, |value| {
        again.push(value);
        true
    }));
    assert_eq!(again, [12]);
}

#[test]
fn a_removed_device_counts_no_frame_as_done_and_records_why() {
    let ring = ExportRing::detached(64, 2);
    assert_eq!(ring.completed(7), 7);
    assert_eq!(ring.fallback(), None, "an ordinary fence value is kept");
    assert_eq!(ring.completed(export::REMOVED), 0);
    assert_eq!(ring.fallback(), Some(Fallback::DeviceRemoved));
    // With nothing counted as done, no frame is ready and no written texture is free.
    let frame = exported(&ring, 0, 1);
    assert!(frame.ring().completed(export::REMOVED) < frame.ready());
    let mut slots = SlotTable::default();
    let slot = slots.pick_released(0).unwrap();
    slots.fill_released(slot, Instant::now()).unwrap();
    assert_ne!(slots.pick_released(ring.completed(export::REMOVED)), Some(slot));
}

#[test]
fn a_frame_given_again_is_refused_while_it_waits_and_after_its_texture_went_back() {
    let ring = ExportRing::detached(64, 2);
    let mut feed = Feed::default();
    let first = exported(&ring, 0, 1);
    assert!(feed.offer(first.clone(), 100).is_none());
    // The same frame again while it waits.
    assert!(!feed.accepts(&first));
    assert_eq!(feed.offer(first.clone(), 101).map(|f| f.ready()), Some(1));
    assert_eq!(feed.waiting(), 1, "the copy is not queued");
    ring.set_ready(1);
    assert!(feed.take_ready().is_some());
    assert_eq!(feed.out(1), 1);
    // Its texture went back and the compositor wrote frame 4 into it: the old frame is refused.
    let newer = exported(&ring, 0, 4);
    assert!(feed.offer(newer, 400).is_none());
    assert!(!feed.accepts(&first));
    assert_eq!(feed.offer(first, 102).map(|f| f.ready()), Some(1));
    assert_eq!(feed.waiting(), 1);
    // A frame older than the newest offered is refused too.
    assert!(!feed.accepts(&exported(&ring, 1, 3)));
    // A session on a ring read before refuses the frames that went back before it started.
    ring.set_ready(4);
    ring.release(4);
    let feed = Feed::default();
    assert!(!feed.accepts(&exported(&ring, 0, 4)));
    assert!(feed.accepts(&exported(&ring, 1, 5)));
}

#[test]
fn a_session_on_a_ring_refuses_a_frame_whose_release_is_queued_but_not_finished() {
    let ring = ExportRing::detached(64, 2);
    ring.hold_releases.store(true, std::sync::atomic::Ordering::Release);
    ring.set_ready(2);
    let mut feed = Feed::default();
    for value in 1..=2 {
        assert!(feed.offer(exported(&ring, value as usize - 1, value), value as i64).is_none());
        assert!(feed.take_ready().is_some());
    }
    assert_eq!(feed.out(2), 2);
    assert_eq!(ring.released(), 0, "the release is queued and has not run");
    // A replacement session gets a copy of frame 2: its texture may be written again any moment.
    let mut next = Feed::default();
    assert!(!next.accepts(&exported(&ring, 1, 2)));
    assert_eq!(next.offer(exported(&ring, 1, 2), 2).map(|f| f.ready()), Some(2));
    assert!(next.accepts(&exported(&ring, 2, 3)));
    ring.finish_releases();
    assert_eq!(ring.released(), 2);
}

#[test]
fn a_stopped_encoders_textures_go_back_once_their_copies_finish() {
    let ring = ExportRing::detached(64, 2);
    let mut feed = Feed::default();
    for value in 1..=SLOTS as u64 {
        assert!(feed.offer(exported(&ring, value as usize - 1, value), value as i64).is_none());
    }
    // Every copy is still running when the encoder stops.
    assert_eq!(feed.drain(), SLOTS);
    assert_eq!(ring.released(), 0, "a texture being written is not handed back");
    assert_eq!(ring.handed_back(), SLOTS as u64, "no later session takes those frames");
    ring.reclaim();
    assert_eq!(ring.released(), 0, "the copies have not finished");
    ring.set_ready(SLOTS as u64 - 1);
    ring.reclaim();
    assert_eq!(ring.released(), 0, "the newest copy has not finished");
    // Every copy finished: the compositor gets the whole ring back.
    ring.set_ready(SLOTS as u64);
    ring.reclaim();
    assert_eq!(ring.released(), SLOTS as u64);
    let mut slots = SlotTable::default();
    for _ in 0..SLOTS {
        let slot = slots.pick_released(0).unwrap();
        slots.fill_released(slot, Instant::now()).unwrap();
    }
    assert!(slots.pick_released(ring.released()).is_some());
}

#[test]
fn a_removed_device_on_the_compositors_side_counts_no_captured_frame_as_done() {
    let ring = SharedRing::detached(64, 2);
    assert_eq!(ring.completed(7), 7);
    assert_eq!(ring.fallback(), None, "an ordinary fence value is kept");
    assert_eq!(ring.completed(export::REMOVED), 0);
    assert_eq!(ring.fallback(), Some(Fallback::DeviceRemoved));
    let mut handoff = Handoff::default();
    handoff.offer(frame(&ring, 0, 1));
    assert!(handoff.take_ready(ring.completed(export::REMOVED)).is_none());
}

#[test]
fn an_imported_handle_gets_a_memory_type_both_it_and_the_image_allow() {
    use crate::fallback::memory_type;
    // The image allows types 0 and 1, the handle only type 1.
    assert_eq!(memory_type(0b11, 0b10, [true, true]), Some(1));
    assert_eq!(memory_type(0b11, 0b11, [true, true]), Some(0));
    // Only device-local types count.
    assert_eq!(memory_type(0b111, 0b111, [false, false, true]), Some(2));
    assert_eq!(memory_type(0b01, 0b10, [true, true]), None, "no type is in both masks");
    assert_eq!(memory_type(0b10, 0b10, [true]), None, "a type the device does not list");
}

#[test]
fn the_export_copy_needs_a_luma_plane_of_the_rings_size_and_a_chroma_plane_of_half() {
    use crate::export::planes_fit;
    assert!(planes_fit(64, 2, (64, 2), (32, 1)));
    assert!(!planes_fit(64, 2, (128, 2), (32, 1)), "a luma plane wider than the ring");
    assert!(!planes_fit(64, 2, (64, 2), (64, 2)), "a chroma plane of the full size");
    assert!(!planes_fit(64, 2, (64, 4), (32, 2)));
}

#[test]
fn a_vulkan_device_without_external_memory_or_timeline_semaphores_says_which_is_needed() {
    let message = Fallback::MissingExtension.to_string();
    assert!(message.contains("shared Windows textures") && message.contains("timeline semaphores"), "{message}");
}

#[test]
fn packets_hand_back_frames_by_count_even_when_two_frames_share_a_time() {
    let ring = ExportRing::detached(64, 2);
    let mut feed = Feed::default();
    ring.set_ready(u64::MAX);
    for value in 1..=2 {
        assert!(feed.offer(exported(&ring, value as usize - 1, value), 7).is_none());
        assert!(feed.take_ready().is_some());
    }
    assert_eq!(feed.out(1), 1, "the first packet is out; the second frame is still being read");
    assert_eq!((ring.released(), feed.reading()), (1, 1));
    assert_eq!(feed.out(1), 1);
    assert_eq!((ring.released(), feed.reading()), (2, 0));
}

#[cfg(feature = "wgpu")]
mod gpu {
    use super::*;

    fn device() -> Option<(wgpu::Device, wgpu::Queue)> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())) else {
            eprintln!("no graphics adapter: this test is skipped");
            return None;
        };
        pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())).ok()
    }

    fn texture(device: &wgpu::Device, w: u32, h: u32, usage: wgpu::TextureUsages) -> wgpu::Texture {
        device.create_texture(&wgpu::TextureDescriptor {
            label: None,
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Bgra8Unorm,
            usage,
            view_formats: &[],
        })
    }

    /// Read a 64 x 1 BGRA texture back (64 texels make a 256-byte row).
    fn read(device: &wgpu::Device, queue: &wgpu::Queue, tex: &wgpu::Texture) -> Vec<u8> {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: None,
            size: 256,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        enc.copy_texture_to_buffer(
            tex.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256), rows_per_image: Some(1) },
            },
            wgpu::Extent3d { width: 64, height: 1, depth_or_array_layers: 1 },
        );
        queue.submit([enc.finish()]);
        buffer.map_async(wgpu::MapMode::Read, .., |r| r.unwrap());
        device.poll(wgpu::PollType::wait_indefinitely()).unwrap();
        buffer.get_mapped_range(..).unwrap().to_vec()
    }

    #[test]
    fn the_opaque_copy_keeps_the_color_and_sets_alpha() {
        let Some((device, queue)) = device() else { return };
        let src = texture(&device, 64, 1, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
        let dst = texture(&device, 64, 1, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        let mut texels = Vec::new();
        for i in 0..64u8 {
            texels.extend([i, i.wrapping_mul(3), 255 - i, i % 2 * 255]);
        }
        queue.write_texture(
            src.as_image_copy(),
            &texels,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256), rows_per_image: Some(1) },
            wgpu::Extent3d { width: 64, height: 1, depth_or_array_layers: 1 },
        );
        let mut opaque = Opaque::new(&device);
        let bind = opaque.bind(&device, &src.create_view(&Default::default()));
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        opaque.draw(&device, &mut enc, &bind, &dst.create_view(&Default::default()), wgpu::TextureFormat::Bgra8Unorm);
        queue.submit([enc.finish()]);
        let out = read(&device, &queue, &dst);
        for (i, (got, want)) in out.chunks(4).zip(texels.chunks(4)).enumerate() {
            assert_eq!(&got[..3], &want[..3], "texel {i} keeps its color");
            assert_eq!(got[3], 255, "texel {i} is opaque");
        }
    }

    #[test]
    fn a_window_picture_keeps_its_alpha_and_a_display_picture_does_not() {
        assert!(!SharedRing::detached(4, 2).keeps_alpha(), "a display's ring is drawn opaque");
        let Some((device, queue)) = device() else { return };
        let src = texture(&device, 64, 1, wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST);
        let dst = texture(&device, 64, 1, wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC);
        // Premultiplied texels: opaque, half covered, and one with no coverage.
        let mut texels = Vec::new();
        let mut want = Vec::new();
        for i in 0..64u8 {
            let alpha: u8 = match i {
                0 => 0,
                _ if i % 2 == 0 => 255,
                _ => 128,
            };
            let straight = if alpha == 0 { [0, 0, 0] } else { [i.wrapping_mul(4), 255 - i, i.wrapping_mul(3)] };
            let pre = straight.map(|c| ((u32::from(c) * u32::from(alpha) + 127) / 255) as u8);
            texels.extend([pre[0], pre[1], pre[2], alpha]);
            want.push([straight[0], straight[1], straight[2], alpha]);
        }
        queue.write_texture(
            src.as_image_copy(),
            &texels,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(256), rows_per_image: Some(1) },
            wgpu::Extent3d { width: 64, height: 1, depth_or_array_layers: 1 },
        );
        let mut opaque = Opaque::new(&device);
        let bind = opaque.bind(&device, &src.create_view(&Default::default()));
        let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
        opaque.draw_keeping_alpha(&device, &mut enc, &bind, &dst.create_view(&Default::default()), wgpu::TextureFormat::Bgra8Unorm);
        queue.submit([enc.finish()]);
        let out = read(&device, &queue, &dst);
        for (i, (got, want)) in out.chunks(4).zip(&want).enumerate() {
            assert_eq!(got[3], want[3], "texel {i} keeps its alpha");
            for c in 0..3 {
                // Undoing the multiplication of an 8-bit value is exact to within 2 steps.
                assert!(got[c].abs_diff(want[c]) <= 2, "texel {i} has straight color: {got:?}, want {want:?}");
            }
        }
    }

    #[test]
    fn a_ring_the_device_cannot_open_records_why_and_drops_the_frame() {
        let Some((device, queue)) = device() else { return };
        let ring = SharedRing::detached(64, 1);
        let format = wgpu::TextureFormat::Bgra8Unorm;
        let asked = std::cell::Cell::new(0);
        let target = || {
            asked.set(asked.get() + 1);
            Some(texture(&device, 64, 1, wgpu::TextureUsages::RENDER_ATTACHMENT).create_view(&Default::default()))
        };
        let mut importer = Importer::new();
        importer.poll();
        assert_eq!(importer.draw(&device, &queue, format, |_, _| target()), Ok(Drawn::Idle));
        importer.offer(frame(&ring, 0, 1));
        let reason = importer.draw(&device, &queue, format, |_, _| target()).unwrap_err();
        assert_eq!(ring.fallback(), Some(reason));
        assert!(importer.pending().is_none());
        // A later frame of the same ring is dropped with the recorded reason, without another
        // attempt to open it.
        importer.offer(frame(&ring, 1, 2));
        assert_eq!(importer.draw(&device, &queue, format, |_, _| target()), Err(reason));
        assert_eq!(importer.draw(&device, &queue, format, |_, _| target()), Ok(Drawn::Idle));
        assert_eq!(asked.get(), 0, "no target is made for a frame that is not drawn");
    }

    #[test]
    fn the_copy_that_readies_the_planes_uses_their_own_formats() {
        let Some((device, queue)) = device() else { return };
        let plane = |format, w, h| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
        };
        // The converter's NV12 planes.
        let planes = [plane(wgpu::TextureFormat::R8Uint, 64, 2), plane(wgpu::TextureFormat::Rg8Uint, 32, 1)];
        let formats = |s: &[wgpu::Texture]| s.iter().map(wgpu::Texture::format).collect::<Vec<_>>();
        let scratch = crate::export::scratch_for(&device, &planes, None);
        assert_eq!(formats(&scratch), formats(&planes));
        let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut encoder = device.create_command_encoder(&Default::default());
        for (p, s) in planes.iter().zip(&scratch) {
            encoder.copy_texture_to_texture(p.as_image_copy(), s.as_image_copy(), wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 });
        }
        queue.submit([encoder.finish()]);
        assert!(pollster::block_on(validation.pop()).is_none(), "each copy is valid");
        // Planes of other formats get new scratch textures; the same formats keep them.
        // A wgpu texture compares equal only to itself and its clones.
        let unorm = [plane(wgpu::TextureFormat::R8Unorm, 64, 2), plane(wgpu::TextureFormat::Rg8Unorm, 32, 1)];
        let uint_scratch = scratch.clone();
        let scratch = crate::export::scratch_for(&device, &unorm, Some(scratch));
        assert_eq!(formats(&scratch), formats(&unorm));
        assert!(scratch.iter().zip(&uint_scratch).all(|(new, old)| new != old), "other formats get new textures");
        let kept = scratch.clone();
        assert_eq!(crate::export::scratch_for(&device, &unorm, Some(scratch)), kept, "the same formats keep the textures");
    }

    #[cfg(not(windows))]
    #[test]
    fn elsewhere_than_windows_an_export_reports_the_fallback() {
        let Some((device, queue)) = device() else { return };
        assert_eq!(device_backend(&device), (Backend::Other, None));
        assert_eq!(Exporter::open(&device, &queue, 64, 2).err(), Some(Fallback::Unsupported));
        assert_eq!(Exporter::open(&device, &queue, 63, 2).err(), Some(Fallback::ImportFailed));
    }

    /// On a Windows device that can share textures, the export ring opens and a frame's planes are
    /// copied into a free texture; anywhere else the reason comes back.
    #[cfg(windows)]
    #[test]
    fn a_windows_compositor_exports_its_frames_or_says_why_not() {
        let Some((device, queue)) = device() else { return };
        let (backend, adapter) = device_backend(&device);
        let mut exporter = match Exporter::open(&device, &queue, 64, 2) {
            Ok(e) => e,
            Err(reason) => {
                // The preflight's reason when it fails; otherwise only opening the ring's
                // resources can fail.
                match check_export(backend, adapter) {
                    Err(expected) => assert_eq!(reason, expected),
                    Ok(_) => assert!(matches!(reason, Fallback::Unsupported | Fallback::ImportFailed), "{reason:?}"),
                }
                return;
            }
        };
        let plane = |format, w, h| {
            device.create_texture(&wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage: wgpu::TextureUsages::COPY_SRC | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        let planes = [plane(wgpu::TextureFormat::R8Unorm, 64, 2), plane(wgpu::TextureFormat::Rg8Unorm, 32, 1)];
        let mut encoder = device.create_command_encoder(&Default::default());
        exporter.prepare(&device, &mut encoder, &planes);
        let (commands, ticket) = exporter.copy(&device, &planes, Instant::now()).unwrap().unwrap();
        queue.submit([encoder.finish(), commands]);
        let frame = exporter.submitted(ticket).unwrap();
        assert_eq!((frame.width(), frame.height(), frame.ready()), (64, 2, 1));
        // The encoder refused the session: later frames come back with its reason.
        exporter.ring().refuse(Fallback::NoEncoder);
        assert_eq!(exporter.copy(&device, &planes, Instant::now()).err(), Some(Fallback::NoEncoder));
    }

    /// A device on wgpu's Vulkan backend, opened with those of `features` the adapter has.
    #[cfg(windows)]
    fn vulkan(features: wgpu::Features) -> Option<(wgpu::Device, wgpu::Queue)> {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::VULKAN;
        let instance = wgpu::Instance::new(desc);
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())) else {
            eprintln!("no Vulkan adapter: this test is skipped");
            return None;
        };
        let desc = wgpu::DeviceDescriptor { required_features: features & adapter.features(), ..Default::default() };
        pollster::block_on(adapter.request_device(&desc)).ok()
    }

    #[cfg(windows)]
    #[test]
    fn a_vulkan_compositor_opens_rings_of_its_own_adapter() {
        let Some((device, queue)) = vulkan(wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32) else { return };
        let (Some(own), timeline) = crate::vulkan_ffi::adapter(&device).expect("a Vulkan device") else {
            eprintln!("the Vulkan driver does not report its adapter: this test is skipped");
            return;
        };
        // A ring of another adapter is refused for the adapter, not for the backend.
        let other = SharedRing::detached_on(64, 1, AdapterId(own.0 ^ 1));
        let mut importer = Importer::new();
        importer.offer(frame(&other, 0, 1));
        assert_eq!(importer.draw(&device, &queue, wgpu::TextureFormat::Bgra8Unorm, |_, _| None), Err(Fallback::OtherAdapter));
        assert_eq!(other.fallback(), Some(Fallback::OtherAdapter));
        // A ring of its own adapter gets past both checks; this one has no shared handles to open.
        let ring = SharedRing::detached_on(64, 1, own);
        let expected = if timeline && device.features().contains(wgpu::Features::VULKAN_EXTERNAL_MEMORY_WIN32) {
            Fallback::ImportFailed
        } else {
            Fallback::MissingExtension
        };
        assert_eq!(crate::vulkan_ffi::import(&device, &queue, &ring).err(), Some(expected));
        // A device opened without the extension cannot open the textures.
        let Some((plain, plain_queue)) = vulkan(wgpu::Features::empty()) else { return };
        assert_eq!(crate::vulkan_ffi::import(&plain, &plain_queue, &ring).err(), Some(Fallback::MissingExtension));
    }

    /// The export image is one non-disjoint NV12 image, so its ownership barriers name the color
    /// aspect and cover both planes.
    #[cfg(windows)]
    #[test]
    fn the_export_ownership_barriers_name_the_color_aspect_of_the_whole_image() {
        use ash::vk;
        let [acquire, hand_back] = crate::vulkan_ffi::export_barriers(vk::Image::null(), 3);
        for barrier in [acquire, hand_back] {
            assert_eq!(barrier.subresource_range.aspect_mask, vk::ImageAspectFlags::COLOR);
            assert_eq!((barrier.subresource_range.level_count, barrier.subresource_range.layer_count), (1, 1));
        }
        assert_eq!((acquire.src_queue_family_index, acquire.dst_queue_family_index), (vk::QUEUE_FAMILY_EXTERNAL, 3));
        assert_eq!((acquire.old_layout, acquire.new_layout), (vk::ImageLayout::UNDEFINED, vk::ImageLayout::TRANSFER_DST_OPTIMAL));
        assert_eq!((hand_back.src_queue_family_index, hand_back.dst_queue_family_index), (3, vk::QUEUE_FAMILY_EXTERNAL));
        assert_eq!((hand_back.old_layout, hand_back.new_layout), (vk::ImageLayout::TRANSFER_DST_OPTIMAL, vk::ImageLayout::GENERAL));
    }

    #[cfg(windows)]
    #[test]
    fn a_direct3d_12_device_is_not_opened_as_vulkan() {
        let mut desc = wgpu::InstanceDescriptor::new_without_display_handle();
        desc.backends = wgpu::Backends::DX12;
        let instance = wgpu::Instance::new(desc);
        let Ok(adapter) = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())) else {
            eprintln!("no Direct3D 12 adapter: this test is skipped");
            return;
        };
        let Ok((device, queue)) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor::default())) else { return };
        assert!(crate::vulkan_ffi::adapter(&device).is_none());
        let ring = SharedRing::detached(64, 1);
        assert_eq!(crate::vulkan_ffi::import(&device, &queue, &ring).err(), Some(Fallback::UnsupportedBackend));
    }
}
