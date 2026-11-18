//! Sharing and publishing neither allocate nor free memory in steady state. This file holds one
//! test, so no other test in the same process allocates while it counts.

use std::alloc::System;

use craft_packet::{Fanout, Info, PacketPool, Receiver, RecvError, RingConfig};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

const SUBSCRIBERS: usize = 8;
const VIDEO_BYTES: usize = 12_500;
const AUDIO_BYTES: usize = 426;

/// One second at 60 fps with an audio packet beside each frame and a keyframe every 2 s. The video
/// buffers are swapped in with `share`, which hands back a buffer for the next frame, and the
/// audio is copied with `share_copy`, both from one pool. The fast subscribers drain their rings
/// on this thread. The slow one never does, so it holds a full ring of packets and keeps dropping.
fn second(
    second: i64,
    pool: &mut PacketPool,
    fanout: &mut Fanout,
    fast: &mut [Receiver],
    video: &mut Vec<u8>,
    audio: &[u8],
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut delivered = 0;
    for frame in 0..60 {
        let t = second * 1000 + frame * 1000 / 60;
        video.resize(VIDEO_BYTES, 0x41);
        let v = fanout.share(pool, video, Info::video(t, t, frame == 0 && second % 2 == 0))?;
        fanout.publish(&v);
        drop(v);
        let a = fanout.share_copy(pool, audio, Info::audio(t))?;
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
fn sharing_and_publishing_allocate_nothing_in_steady_state() {
    let ring = RingConfig { slots: 360, bytes: 3_500_000, span: 3000, video: true, wake_every: 1 };
    let mut fanout = Fanout::new();
    let mut fast: Vec<_> = (0..SUBSCRIBERS - 1).map(|_| fanout.subscribe(ring).unwrap()).collect();
    // Room for about eight seconds, which the warm-up fills.
    let slow = fanout.subscribe(RingConfig { slots: 1000, bytes: 20_000_000, ..ring }).unwrap();
    // The pool starts with four empty packets and makes the rest while it warms up.
    let mut pool = PacketPool::new(4, 0).unwrap();
    let mut video = Vec::new();
    let audio = vec![0x21u8; AUDIO_BYTES];
    for s in 0..12 {
        second(s, &mut pool, &mut fanout, &mut fast, &mut video, &audio).unwrap();
    }
    let made = pool.made();
    assert!(made > 1000, "the pool made {made} packets");
    let region = Region::new(GLOBAL);
    let mut delivered = 0;
    for s in 12..72 {
        delivered += second(s, &mut pool, &mut fanout, &mut fast, &mut video, &audio).unwrap();
    }
    let change = region.change();
    assert_eq!(change.allocations, 0, "allocations while publishing: {change:?}");
    assert_eq!(change.reallocations, 0, "reallocations while publishing: {change:?}");
    assert_eq!(change.deallocations, 0, "deallocations while publishing: {change:?}");
    assert_eq!(pool.made(), made);
    assert_eq!(delivered, 60 * 120 * (SUBSCRIBERS - 1));
    let st = slow.stats();
    assert_eq!(st.queued_packets, 1000, "the slow subscriber holds a full ring");
    assert!(st.dropped_packets > 0, "the slow subscriber overflowed");
}
