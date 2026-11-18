//! Publishing one second of a 6000 kb/s, 60 fps stream with 47 audio packets (107 packets) to 1,
//! 4, 8 and 16 subscribers, one of which never reads, so its ring stays full and it drops packets
//! up to each keyframe. The other subscribers' rings are drained between seconds, outside the
//! timing. A counting allocator checks that the timed part allocates nothing; the bench exits with
//! an error when it does.

use std::alloc::System;
use std::hint::black_box;
use std::time::{Duration, Instant};

use craft_packet::{Fanout, Info, PacketPool, Receiver, RingConfig};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

const VIDEO_BYTES: usize = 12_500;
const AUDIO_BYTES: usize = 426;
const AUDIO_PACKETS: i64 = 47;
const PACKETS: u64 = 60 + AUDIO_PACKETS as u64;

fn fail(why: &str) -> ! {
    eprintln!("fanout bench: {why}");
    std::process::exit(1)
}

/// One second of packets, in decoding order, made once and published every iteration. A keyframe
/// opens the second.
fn one_second(pool: &mut PacketPool) -> Vec<craft_packet::SharedPacket> {
    let video = vec![0x41u8; VIDEO_BYTES];
    let audio = vec![0x21u8; AUDIO_BYTES];
    let mut share = |data: &[u8], info| pool.share_copy(data, info).unwrap_or_else(|_| fail("the pool refused a packet"));
    let mut out = Vec::new();
    let mut a = 0;
    for f in 0..60i64 {
        let t = f * 1000 / 60;
        out.push(share(&video, Info::video(t, t, f == 0)));
        while a < AUDIO_PACKETS && a * 1024 * 1000 / 48_000 <= t {
            out.push(share(&audio, Info::audio(a * 1024 * 1000 / 48_000)));
            a += 1;
        }
    }
    while a < AUDIO_PACKETS {
        out.push(share(&audio, Info::audio(a * 1024 * 1000 / 48_000)));
        a += 1;
    }
    out
}

fn drain(rs: &mut [Receiver]) {
    for r in rs {
        while r.try_recv().is_ok() {}
    }
}

fn bench(c: &mut Criterion) {
    let mut pool = PacketPool::new(128, VIDEO_BYTES).unwrap_or_else(|_| fail("the pool was refused"));
    let second = one_second(&mut pool);
    // The timestamps repeat each second, so the span is large enough that nothing is stale.
    let ring = RingConfig { slots: 330, bytes: 3_500_000, span: i64::MAX, video: true };
    let mut g = c.benchmark_group("fanout");
    g.throughput(Throughput::Elements(PACKETS));
    let mut allocations = 0;
    for n in [1usize, 4, 8, 16] {
        let mut fanout = Fanout::new();
        let slow = fanout.subscribe(RingConfig { slots: 16, ..ring }).unwrap_or_else(|_| fail("the slow ring was refused"));
        let mut fast: Vec<Receiver> = (1..n).map(|_| fanout.subscribe(ring).unwrap_or_else(|_| fail("a ring was refused"))).collect();
        // Warm up the subscriber list's read path and the rings.
        for p in &second {
            fanout.publish(p);
        }
        drain(&mut fast);
        let name = if n == 1 {
            "publish 1 s to 1 subscriber that never reads".to_string()
        } else {
            format!("publish 1 s to {n} subscribers, one never reads")
        };
        g.bench_function(name, |b| {
            b.iter_custom(|iters| {
                let mut timed = Duration::ZERO;
                for _ in 0..iters {
                    let region = Region::new(GLOBAL);
                    let start = Instant::now();
                    let mut took = 0;
                    for p in &second {
                        took += fanout.publish(black_box(p));
                    }
                    timed += start.elapsed();
                    allocations += region.change().allocations;
                    black_box(took);
                    drain(&mut fast);
                }
                timed
            })
        });
        if slow.stats().dropped_packets == 0 {
            fail("the subscriber that never reads dropped nothing");
        }
    }
    g.finish();
    if allocations > 0 {
        fail(&format!("publishing allocated {allocations} times"));
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
