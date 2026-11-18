//! Publishing allocates nothing in steady state. This file holds one test, so no other test in the
//! same process allocates while it counts.

use std::alloc::System;

use craft_packet::{Fanout, Info, PacketPool, RecvError, RingConfig};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

const SUBSCRIBERS: usize = 8;
const VIDEO_BYTES: usize = 12_500;
const AUDIO_BYTES: usize = 426;

/// One second at 60 fps with an audio packet beside each frame and a keyframe every 2 s: the video
/// buffers are swapped in with `share`, which hands back a buffer for the next frame; the audio is
/// copied with
/// `share_copy`. The fast subscribers drain their rings on this thread; the slow one never does,
/// so it keeps overflowing and resyncing.
fn second(
    second: i64,
    pool: &mut PacketPool,
    fanout: &mut Fanout,
    fast: &mut [craft_packet::Receiver],
    video: &mut Vec<u8>,
    audio: &[u8],
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut delivered = 0;
    for frame in 0..60 {
        let t = second * 1000 + frame * 1000 / 60;
        video.resize(VIDEO_BYTES, 0x41);
        let v = pool.share(video, Info::video(t, t, frame == 0 && second % 2 == 0))?;
        fanout.publish(&v);
        drop(v);
        let a = pool.share_copy(audio, Info::audio(t))?;
        fanout.publish(&a);
        drop(a);
        for r in fast.iter_mut() {
            loop {
                match r.try_recv() {
                    Ok(_) => delivered += 1,
                    Err(RecvError::Empty) => break,
                    Err(e) => return Err(e.into()),
                }
            }
        }
    }
    Ok(delivered)
}

#[test]
fn publishing_allocates_nothing_in_steady_state() {
    let ring = RingConfig { slots: 360, bytes: 3_500_000, span: 3000, video: true };
    let mut fanout = Fanout::new();
    let mut fast: Vec<_> = (0..SUBSCRIBERS - 1).map(|_| fanout.subscribe(ring).unwrap()).collect();
    let slow = fanout.subscribe(RingConfig { slots: 16, ..ring }).unwrap();
    let mut pool = PacketPool::new(32, VIDEO_BYTES).unwrap();
    let mut video = Vec::with_capacity(VIDEO_BYTES);
    let audio = vec![0x21u8; AUDIO_BYTES];
    // Warm up: the slow ring fills and holds packets, so the pool makes as many as are in flight.
    for s in 0..4 {
        second(s, &mut pool, &mut fanout, &mut fast, &mut video, &audio).unwrap();
    }
    let made = pool.made();
    let region = Region::new(GLOBAL);
    let mut delivered = 0;
    for s in 4..64 {
        delivered += second(s, &mut pool, &mut fanout, &mut fast, &mut video, &audio).unwrap();
    }
    let change = region.change();
    assert_eq!(change.allocations, 0, "allocations while publishing: {change:?}");
    assert_eq!(change.reallocations, 0, "reallocations while publishing: {change:?}");
    assert_eq!(pool.made(), made);
    assert_eq!(delivered, 60 * 120 * (SUBSCRIBERS - 1));
    assert!(slow.stats().dropped_packets > 0, "the slow subscriber overflowed");
}
