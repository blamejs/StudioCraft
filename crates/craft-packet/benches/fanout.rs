//! The producer's cost for one second of a 6000 kb/s, 60 fps stream with 47 audio packets (107
//! packets), in three groups.
//!
//! - `fanout`: publishing to 1, 4, 8 and 16 subscribers, one of which never reads, so its ring
//!   stays full and it drops packets up to each keyframe. The other rings are drained between
//!   seconds on the bench thread, outside the timing.
//! - `parked`: publishing to 1, 4, 8 and 16 receivers that each run on their own thread and park
//!   in `recv_timeout` between packets, as a network sender that keeps up does. Before each packet
//!   the producer waits, outside the timing, until every receiver is parked, so each publish pays
//!   for waking the receivers it wakes. Two cases wake the receivers every 3 packets, as
//!   `RingConfig::for_stream` sets for this stream.
//! - `share`: sharing each packet from the pool (video swapped in with `share`, audio copied with
//!   `share_copy`) and publishing it to one subscriber, with and without a stalled subscriber whose
//!   ring holds 10 000 packets.
//!
//! Every second's packets carry new timestamps. A counting allocator checks the timed parts; the
//! bench exits with an error when one allocates.

use std::alloc::System;
use std::hint::black_box;
use std::thread;
use std::time::{Duration, Instant};

use craft_packet::{Fanout, Info, Monitor, PacketPool, Receiver, RingConfig, SharedPacket};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

const VIDEO_BYTES: usize = 12_500;
const AUDIO_BYTES: usize = 426;
const AUDIO_PACKETS: i64 = 47;
const PACKETS: u64 = 60 + AUDIO_PACKETS as u64;
/// Three seconds, in the stream's millisecond time base.
const SPAN: i64 = 3000;

fn fail(why: &str) -> ! {
    eprintln!("fanout bench: {why}");
    std::process::exit(1)
}

/// The packets of second `s` in decoding order, as (is video, timestamp). A keyframe opens the
/// second.
fn schedule() -> Vec<(bool, i64)> {
    let mut out = Vec::new();
    let mut a = 0;
    for f in 0..60i64 {
        let t = f * 1000 / 60;
        out.push((true, t));
        while a < AUDIO_PACKETS && a * 1024 * 1000 / 48_000 <= t {
            out.push((false, a * 1024 * 1000 / 48_000));
            a += 1;
        }
    }
    while a < AUDIO_PACKETS {
        out.push((false, a * 1024 * 1000 / 48_000));
        a += 1;
    }
    out
}

fn info(video: bool, t: i64) -> Info {
    if video { Info::video(t, t, t % 1000 == 0) } else { Info::audio(t) }
}

/// Second `s`'s packets, copied into `out` from `pool`, outside any timing.
fn make_second(pool: &mut PacketPool, sched: &[(bool, i64)], s: i64, out: &mut Vec<SharedPacket>) {
    let video = vec![0x41u8; VIDEO_BYTES];
    let audio = vec![0x21u8; AUDIO_BYTES];
    out.clear();
    for &(v, t) in sched {
        let data = if v { &video } else { &audio };
        out.push(pool.share_copy(data, info(v, s * 1000 + t)).unwrap_or_else(|_| fail("the pool refused a packet")));
    }
}

fn drain(rs: &mut [Receiver]) {
    for r in rs {
        while r.try_recv().is_ok() {}
    }
}

fn ring(slots: usize) -> RingConfig {
    RingConfig { slots, bytes: 3_500_000, span: SPAN, video: true, wake_every: 1 }
}

fn fanout(c: &mut Criterion, allocations: &mut u64) {
    let sched = schedule();
    let mut pool = PacketPool::new(1024, VIDEO_BYTES).unwrap_or_else(|_| fail("the pool was refused"));
    let mut second = Vec::with_capacity(sched.len());
    let mut g = c.benchmark_group("fanout");
    g.throughput(Throughput::Elements(PACKETS));
    let mut s = 0;
    for n in [1usize, 4, 8, 16] {
        let mut fanout = Fanout::new();
        let slow = fanout.subscribe(ring(16)).unwrap_or_else(|_| fail("the slow ring was refused"));
        let mut fast: Vec<Receiver> = (1..n).map(|_| fanout.subscribe(ring(330)).unwrap_or_else(|_| fail("a ring was refused"))).collect();
        let name = if n == 1 {
            "publish 1 s to 1 subscriber that never reads".to_string()
        } else {
            format!("publish 1 s to {n} subscribers, one never reads")
        };
        g.bench_function(name, |b| {
            b.iter_custom(|iters| {
                let mut timed = Duration::ZERO;
                for _ in 0..iters {
                    make_second(&mut pool, &sched, s, &mut second);
                    s += 1;
                    let region = Region::new(GLOBAL);
                    let start = Instant::now();
                    let mut took = 0;
                    for p in &second {
                        took += fanout.publish(black_box(p));
                    }
                    timed += start.elapsed();
                    *allocations += region.change().allocations as u64;
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
}

/// Waits until every receiver is parked, or about to park, in `recv_timeout`.
fn wait_parked(monitors: &[Monitor]) {
    for m in monitors {
        while !m.is_waiting() {
            std::hint::spin_loop();
        }
    }
}

fn parked(c: &mut Criterion, allocations: &mut u64) {
    let sched = schedule();
    let mut pool = PacketPool::new(1024, VIDEO_BYTES).unwrap_or_else(|_| fail("the pool was refused"));
    let mut second = Vec::with_capacity(sched.len());
    let mut g = c.benchmark_group("parked");
    g.throughput(Throughput::Elements(PACKETS));
    let mut s = 0;
    for (n, wake_every) in [(1usize, 1usize), (4, 1), (8, 1), (16, 1), (8, 3), (16, 3)] {
        let mut fanout = Fanout::new();
        let cfg = RingConfig { wake_every, ..ring(330) };
        let receivers: Vec<Receiver> = (0..n).map(|_| fanout.subscribe(cfg).unwrap_or_else(|_| fail("a ring was refused"))).collect();
        let monitors: Vec<Monitor> = receivers.iter().map(Receiver::monitor).collect();
        let threads: Vec<_> = receivers
            .into_iter()
            .map(|mut r| {
                thread::spawn(move || {
                    let mut n = 0u64;
                    while r.recv_timeout(Duration::from_secs(60)).is_ok() {
                        n += 1;
                    }
                    n
                })
            })
            .collect();
        let name = if n == 1 {
            "publish 1 s to 1 parked receiver".to_string()
        } else if wake_every == 1 {
            format!("publish 1 s to {n} parked receivers")
        } else {
            format!("publish 1 s to {n} parked receivers, woken every {wake_every} packets")
        };
        g.bench_function(name, |b| {
            b.iter_custom(|iters| {
                let mut timed = Duration::ZERO;
                for _ in 0..iters {
                    make_second(&mut pool, &sched, s, &mut second);
                    s += 1;
                    for p in &second {
                        wait_parked(&monitors);
                        let region = Region::new(GLOBAL);
                        let start = Instant::now();
                        black_box(fanout.publish(black_box(p)));
                        timed += start.elapsed();
                        *allocations += region.change().allocations as u64;
                    }
                }
                timed
            })
        });
        let wakes: u64 = monitors.iter().map(|m| m.stats().wakes).sum();
        drop(fanout);
        for t in threads {
            if t.join().unwrap_or(0) == 0 {
                fail("a parked receiver got no packets");
            }
        }
        if wakes == 0 {
            fail("no receiver was woken");
        }
    }
    g.finish();
}

fn share(c: &mut Criterion, allocations: &mut u64) {
    let sched = schedule();
    let mut g = c.benchmark_group("share");
    g.throughput(Throughput::Elements(PACKETS));
    let audio = vec![0x21u8; AUDIO_BYTES];
    for stalled in [0usize, 10_000] {
        let mut pool = PacketPool::new(64, VIDEO_BYTES).unwrap_or_else(|_| fail("the pool was refused"));
        let mut fanout = Fanout::new();
        let mut fast = vec![fanout.subscribe(ring(330)).unwrap_or_else(|_| fail("a ring was refused"))];
        let slow = if stalled > 0 {
            let cfg = RingConfig { slots: stalled, bytes: 128 << 20, span: SPAN, video: true, wake_every: 1 };
            Some(fanout.subscribe(cfg).unwrap_or_else(|_| fail("the stalled ring was refused")))
        } else {
            None
        };
        let mut video = Vec::new();
        let mut s = 0;
        let mut run = |fanout: &mut Fanout, pool: &mut PacketPool, s: i64| {
            for &(v, t) in &sched {
                let p = if v {
                    video.resize(VIDEO_BYTES, 0x41);
                    fanout.share(pool, &mut video, info(v, s * 1000 + t))
                } else {
                    fanout.share_copy(pool, &audio, info(v, s * 1000 + t))
                };
                let p = p.unwrap_or_else(|_| fail("the pool refused a packet"));
                black_box(fanout.publish(&p));
            }
        };
        // Fill the stalled ring and let the pool make the packets that are in flight.
        let warm = 3 + stalled as i64 / PACKETS as i64;
        while s < warm {
            run(&mut fanout, &mut pool, s);
            drain(&mut fast);
            s += 1;
        }
        let name = format!("share and publish 1 s, stalled ring holds {stalled} packets");
        g.bench_function(name, |b| {
            b.iter_custom(|iters| {
                let mut timed = Duration::ZERO;
                for _ in 0..iters {
                    let region = Region::new(GLOBAL);
                    let start = Instant::now();
                    run(&mut fanout, &mut pool, s);
                    timed += start.elapsed();
                    *allocations += region.change().allocations as u64;
                    s += 1;
                    drain(&mut fast);
                }
                timed
            })
        });
        if let Some(slow) = slow
            && slow.stats().queued_packets != stalled
        {
            fail("the stalled ring is not full");
        }
    }
    g.finish();
}

fn bench(c: &mut Criterion) {
    let mut allocations = 0;
    fanout(c, &mut allocations);
    parked(c, &mut allocations);
    share(c, &mut allocations);
    if allocations > 0 {
        fail(&format!("the timed parts allocated {allocations} times"));
    }
}

criterion_group!(benches, bench);
criterion_main!(benches);
