use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::*;

/// A ring with room for `slots` packets and plenty of bytes and span.
fn slots(slots: usize) -> RingConfig {
    RingConfig { slots, bytes: 1 << 30, span: i64::MAX, video: true }
}

/// A test stream: frame `i` is a video packet at `i * 10` ms, a keyframe every `gop` frames, then
/// an audio packet at the same time.
struct Stream {
    pool: PacketPool,
    i: i64,
    gop: i64,
}

impl Stream {
    fn new(gop: i64) -> Self {
        Self { pool: PacketPool::new(8, 64).unwrap(), i: 0, gop }
    }

    /// The next frame's video and audio packets.
    fn frame(&mut self) -> [SharedPacket; 2] {
        let t = self.i * 10;
        let key = self.i % self.gop == 0;
        self.i += 1;
        [self.pool.share_copy(&[1; 10], Info::video(t, t, key)).unwrap(), self.pool.share_copy(&[2; 4], Info::audio(t)).unwrap()]
    }

    fn publish(&mut self, f: &mut Fanout, frames: usize) {
        for _ in 0..frames {
            for p in self.frame() {
                f.publish(&p);
            }
        }
    }
}

fn drain(r: &mut Receiver) -> Vec<SharedPacket> {
    let mut out = Vec::new();
    while let Ok(p) = r.try_recv() {
        out.push(p);
    }
    out
}

#[test]
fn clones_share_the_bytes() {
    let mut pool = PacketPool::new(1, 16).unwrap();
    let a = pool.share_copy(b"frame", Info::video(0, 0, true)).unwrap();
    let b = a.clone();
    assert!(SharedPacket::ptr_eq(&a, &b));
    assert_eq!(a.data().as_ptr(), b.data().as_ptr());
    assert_eq!(b.data(), b"frame");
    assert_eq!(b.info(), Info::video(0, 0, true));
}

#[test]
fn share_moves_the_buffer_in_and_hands_the_old_one_back() {
    let mut pool = PacketPool::new(1, 0).unwrap();
    let mut buf = vec![7u8; 4096];
    let at = buf.as_ptr();
    let p = pool.share(&mut buf, Info::video(0, 0, true)).unwrap();
    assert_eq!(p.data().as_ptr(), at, "the encoder's buffer is not copied");
    assert!(buf.is_empty());
    drop(p);
    let mut next = vec![1, 2, 3];
    let q = pool.share(&mut next, Info::audio(1)).unwrap();
    assert_eq!(next.as_ptr(), at, "the free packet's old buffer comes back");
    assert!(next.is_empty() && next.capacity() >= 4096);
    assert_eq!(q.data(), [1, 2, 3]);
}

#[test]
fn the_pool_reuses_packets_once_they_are_dropped() {
    let mut pool = PacketPool::new(2, 64).unwrap();
    for i in 0..1000 {
        let a = pool.share_copy(&[1; 32], Info::audio(i)).unwrap();
        let b = pool.share_copy(&[2; 32], Info::audio(i)).unwrap();
        drop((a, b));
    }
    assert_eq!(pool.made(), 2);
    let held: Vec<_> = (0..5).map(|i| pool.share_copy(&[3], Info::audio(i)).unwrap()).collect();
    assert_eq!(pool.made(), 5, "packets still in flight are not reused");
    assert_eq!(pool.free(), 0);
    drop(held);
    assert_eq!(pool.free(), 5);
}

#[test]
fn packets_are_numbered_in_order() {
    let mut pool = PacketPool::new(1, 0).unwrap();
    let seqs: Vec<u64> = (0..4).map(|i| pool.share_copy(&[], Info::data(i)).unwrap().seq()).collect();
    assert_eq!(seqs, [1, 2, 3, 4]);
}

#[test]
fn a_full_pool_refuses_and_leaves_the_buffer_alone() {
    let mut pool = PacketPool::new(MAX_SLOTS, 0).unwrap();
    let held: Vec<_> = (0..MAX_SLOTS).map(|i| pool.share_copy(&[], Info::data(i as i64)).unwrap()).collect();
    assert_eq!(pool.share_copy(&[1], Info::data(0)).unwrap_err(), ShareError::PoolFull);
    let mut buf = vec![9u8; 8];
    assert_eq!(pool.share(&mut buf, Info::data(0)).unwrap_err(), ShareError::PoolFull);
    assert_eq!(buf, [9; 8]);
    assert_eq!(pool.made(), MAX_SLOTS);
    drop(held);
    assert_eq!(pool.share(&mut buf, Info::data(0)).map(|p| p.seq()), Ok(MAX_SLOTS as u64 + 1));
}

#[test]
fn the_pool_refuses_buffers_past_its_byte_limit() {
    let mut pool = PacketPool::new(1, 0).unwrap();
    let per = MAX_POOL_BYTES / 16;
    let held: Vec<_> = (0..16)
        .map(|i| {
            let mut buf = Vec::with_capacity(per);
            pool.share(&mut buf, Info::data(i)).unwrap()
        })
        .collect();
    assert_eq!(pool.held_bytes(), MAX_POOL_BYTES);
    let mut extra = Vec::with_capacity(per);
    assert_eq!(pool.share(&mut extra, Info::data(16)).unwrap_err(), ShareError::PoolFull);
    assert_eq!(extra.capacity(), per);
    assert_eq!(pool.share_copy(&[1], Info::data(16)).unwrap_err(), ShareError::PoolFull);
    drop(held);
    assert!(pool.share_copy(&[1], Info::data(16)).is_ok());
}

#[test]
fn a_full_pool_uses_a_free_packet_whose_buffer_is_large_enough() {
    let mut pool = PacketPool::new(1, 0).unwrap();
    let mut small = Vec::with_capacity(1);
    let mut held = vec![pool.share(&mut small, Info::data(0)).unwrap()];
    for i in 0..16 {
        let mut big = Vec::with_capacity(if i == 15 { MAX_BUFFER_BYTES - 1 } else { MAX_BUFFER_BYTES });
        held.push(pool.share(&mut big, Info::data(1)).unwrap());
    }
    assert_eq!(pool.held_bytes(), MAX_POOL_BYTES);
    drop(held);
    // The next free packet has a 1-byte buffer and cannot grow; the one after it can take 4 bytes.
    let p = pool.share_copy(&[7; 4], Info::data(2)).unwrap();
    assert_eq!(p.data(), [7; 4]);
    assert_eq!(pool.held_bytes(), MAX_POOL_BYTES);
}

#[test]
fn share_copy_refuses_a_packet_over_the_buffer_limit() {
    let mut pool = PacketPool::new(1, 0).unwrap();
    let big = vec![0u8; MAX_BUFFER_BYTES + 1];
    assert_eq!(pool.share_copy(&big, Info::data(0)).unwrap_err(), ShareError::TooLarge(MAX_BUFFER_BYTES + 1));
    assert_eq!(pool.share_copy(&big[..MAX_BUFFER_BYTES], Info::data(0)).map(|p| p.len()), Ok(MAX_BUFFER_BYTES));
}

#[test]
fn share_refuses_a_buffer_over_the_buffer_limit() {
    let mut pool = PacketPool::new(1, 0).unwrap();
    let mut big = Vec::with_capacity(MAX_BUFFER_BYTES + 1);
    big.push(5);
    assert_eq!(pool.share(&mut big, Info::data(0)).unwrap_err(), ShareError::BufferTooLarge(MAX_BUFFER_BYTES + 1));
    assert_eq!(big, [5]);
    assert_eq!(pool.held_bytes(), 0);
}

#[test]
fn every_subscriber_gets_every_packet_in_order() {
    let mut f = Fanout::new();
    let mut rs: Vec<Receiver> = (0..3).map(|_| f.subscribe(slots(64)).unwrap()).collect();
    let mut s = Stream::new(5);
    s.publish(&mut f, 10);
    for r in &mut rs {
        let got = drain(r);
        assert_eq!(got.len(), 20);
        assert!(got.windows(2).all(|w| w[1].seq() == w[0].seq() + 1));
        assert_eq!(r.stats().delivered_packets, 20);
        assert_eq!(r.stats().dropped_packets, 0);
    }
}

#[test]
fn a_slow_subscriber_never_blocks_the_publisher_or_the_others() {
    const FRAMES: usize = 5000;
    let mut f = Fanout::new();
    let slow = f.subscribe(slots(8)).unwrap();
    let (ready, started) = mpsc::channel();
    let fast: Vec<_> = (0..3)
        .map(|_| {
            // Room for the whole stream, so a fast subscriber's thread loses nothing however the
            // threads are scheduled.
            let mut r = f.subscribe(slots(FRAMES * 2)).unwrap();
            let ready = ready.clone();
            thread::spawn(move || {
                ready.send(()).unwrap();
                let mut n = 0;
                while r.recv_timeout(Duration::from_secs(60)).is_ok() {
                    n += 1;
                }
                (n, r.stats())
            })
        })
        .collect();
    for _ in 0..3 {
        started.recv().unwrap();
    }
    let mut s = Stream::new(30);
    s.publish(&mut f, FRAMES);
    drop(f);
    for h in fast {
        let (n, st) = h.join().unwrap();
        assert_eq!(n, FRAMES * 2, "a fast subscriber got every packet");
        assert_eq!(st.dropped_packets, 0);
    }
    let st = slow.stats();
    assert!(st.queued_packets <= 8);
    assert_eq!(st.accepted_packets + st.dropped_packets, FRAMES as u64 * 2);
    assert!(st.dropped_packets > 0);
}

#[test]
fn after_an_overflow_the_ring_resumes_at_a_keyframe() {
    let mut f = Fanout::new();
    let mut r = f.subscribe(slots(6)).unwrap();
    let mut s = Stream::new(8);
    // Frames 0 to 2 fit and frame 3 overflows.
    s.publish(&mut f, 4);
    assert!(r.stats().resyncing);
    let first = drain(&mut r);
    assert_eq!(first.len(), 6);
    // The ring has room again, but frames 4 to 7 are dropped up to frame 8's keyframe.
    s.publish(&mut f, 5);
    let next = drain(&mut r);
    assert_eq!(next.len(), 2);
    assert_eq!(next[0].kind(), Kind::Video);
    assert!(next[0].keyframe());
    assert_eq!(next[0].dts(), 80);
    assert!(!r.stats().resyncing);
    assert_eq!(r.stats().dropped_packets, 10);
}

#[test]
fn audio_and_video_stay_aligned_across_a_drop() {
    let mut f = Fanout::new();
    let mut r = f.subscribe(slots(8)).unwrap();
    let mut s = Stream::new(5);
    // Frames 0 to 3 fill the ring. Frame 4 overflows, and frame 5's keyframe finds the ring still
    // full, so the ring resyncs again.
    s.publish(&mut f, 6);
    let mut got = drain(&mut r);
    // Frames 6 to 9 are dropped while the ring waits for frame 10's keyframe.
    s.publish(&mut f, 5);
    got.extend(drain(&mut r));
    assert_eq!(r.stats().dropped_packets, 12, "frames 4 to 9, audio and video alike");
    let gap = got.windows(2).position(|w| w[1].seq() != w[0].seq() + 1).unwrap();
    let (before, after) = got.split_at(gap + 1);
    assert_eq!(before.len(), 8);
    assert!(before.windows(2).all(|w| w[1].seq() == w[0].seq() + 1));
    assert_eq!(before.last().map(|p| (p.kind(), p.dts())), Some((Kind::Audio, 30)), "the run before the gap ends on a whole frame");
    assert_eq!(after.len(), 2);
    assert!(after[0].keyframe());
    assert_eq!(after[0].dts(), 100);
    assert_eq!((after[1].kind(), after[1].dts()), (Kind::Audio, 100), "no audio from the dropped frames is delivered");
}

#[test]
fn the_byte_cap_drops_like_the_packet_cap() {
    let mut f = Fanout::new();
    let mut r = f.subscribe(RingConfig { slots: 100, bytes: 30, span: i64::MAX, video: true }).unwrap();
    let mut s = Stream::new(100);
    // 14 bytes a frame: two frames fit in 30 bytes, the third frame's video packet does not.
    s.publish(&mut f, 3);
    let st = r.stats();
    assert_eq!(st.accepted_packets, 4);
    assert_eq!(st.dropped_packets, 2);
    assert_eq!(st.queued_bytes, 28);
    assert_eq!(drain(&mut r).len(), 4);
    assert_eq!(r.stats().queued_bytes, 0);
}

#[test]
fn an_empty_ring_takes_a_packet_larger_than_its_byte_cap() {
    let mut f = Fanout::new();
    let mut r = f.subscribe(RingConfig { slots: 4, bytes: 8, span: i64::MAX, video: true }).unwrap();
    let mut pool = PacketPool::new(1, 0).unwrap();
    let big = pool.share_copy(&[0; 100], Info::video(0, 0, true)).unwrap();
    assert_eq!(f.publish(&big), 1);
    assert_eq!(drain(&mut r).len(), 1);
}

#[test]
fn the_receiver_skips_stale_packets_to_the_next_keyframe() {
    let mut f = Fanout::new();
    let mut r = f.subscribe(RingConfig { slots: 1000, bytes: 1 << 20, span: 100, video: true }).unwrap();
    let mut s = Stream::new(10);
    // 30 frames span 290 ms; the receiver falls behind by more than 100 ms.
    s.publish(&mut f, 30);
    let got = drain(&mut r);
    assert!(got[0].keyframe());
    // The newest packet is at 290 ms; the first keyframe within 100 ms of it is frame 20.
    assert_eq!(got[0].dts(), 200);
    assert_eq!(got.len(), 20);
    let st = r.stats();
    assert_eq!(st.skipped_packets, 40);
    assert_eq!(st.accepted_packets, st.delivered_packets + st.skipped_packets);
}

#[test]
fn a_new_subscriber_starts_at_the_next_keyframe_without_counting_drops() {
    let mut f = Fanout::new();
    let mut s = Stream::new(4);
    s.publish(&mut f, 2);
    let mut r = f.subscribe(slots(64)).unwrap();
    s.publish(&mut f, 4);
    let got = drain(&mut r);
    assert!(got[0].keyframe());
    assert_eq!(got[0].dts(), 40);
    assert_eq!(r.stats().dropped_packets, 0);
}

#[test]
fn an_audio_only_ring_resumes_at_any_audio_packet() {
    let mut f = Fanout::new();
    let mut r = f.subscribe(RingConfig { slots: 2, bytes: 1 << 20, span: i64::MAX, video: false }).unwrap();
    let mut pool = PacketPool::new(4, 8).unwrap();
    for t in 0..3 {
        f.publish(&pool.share_copy(&[0; 4], Info::audio(t)).unwrap());
    }
    assert_eq!(drain(&mut r).len(), 2);
    f.publish(&pool.share_copy(&[0; 4], Info::audio(3)).unwrap());
    let got = drain(&mut r);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].dts(), 3);
}

#[test]
fn dropping_a_receiver_unsubscribes_it() {
    let mut f = Fanout::new();
    let control = f.control();
    let a = f.subscribe(slots(4)).unwrap();
    let _b = control.subscribe(slots(4)).unwrap();
    assert_eq!(control.subscribers(), 2);
    drop(a);
    assert_eq!(f.subscribers(), 1);
    let mut s = Stream::new(1);
    let [v, _] = s.frame();
    assert_eq!(f.publish(&v), 1);
}

#[test]
fn dropping_a_receiver_releases_its_queued_packets_while_a_monitor_lives() {
    let mut pool = PacketPool::new(1, 64).unwrap();
    let mut f = Fanout::new();
    let r = f.subscribe(slots(8)).unwrap();
    let m = r.monitor();
    f.publish(&pool.share_copy(&[1; 10], Info::video(0, 0, true)).unwrap());
    assert_eq!(pool.free(), 0);
    drop(r);
    assert_eq!(pool.free(), 1, "the pool takes back the queued packet");
    let st = m.stats();
    assert_eq!((st.queued_packets, st.queued_bytes), (0, 0));
    assert_eq!((st.discarded_packets, st.discarded_bytes), (1, 10));
    assert_eq!(st.accepted_packets, st.delivered_packets + st.skipped_packets + st.discarded_packets);
}

#[test]
fn a_packet_pushed_after_the_receiver_is_dropped_is_released() {
    let mut pool = PacketPool::new(1, 64).unwrap();
    let f = Fanout::new();
    let r = f.subscribe(slots(8)).unwrap();
    let m = r.monitor();
    drop(r);
    m.push_in_flight(pool.share_copy(&[1; 10], Info::video(0, 0, true)).unwrap());
    assert_eq!(pool.free(), 1, "the pool takes back the late packet");
    assert_eq!(m.stats().queued_packets, 0);
}

#[test]
fn unsubscribe_closes_the_receiver_after_its_ring_drains() {
    let mut f = Fanout::new();
    let control = f.control();
    let mut r = f.subscribe(slots(8)).unwrap();
    Stream::new(1).publish(&mut f, 1);
    assert!(control.unsubscribe(r.id()));
    assert!(!control.unsubscribe(r.id()));
    assert_eq!(r.try_recv().map(|p| p.kind()), Ok(Kind::Video));
    assert_eq!(r.try_recv().map(|p| p.kind()), Ok(Kind::Audio));
    assert_eq!(r.try_recv().unwrap_err(), RecvError::Closed);
}

#[test]
fn no_packet_follows_closed() {
    let f = Fanout::new();
    let control = f.control();
    let mut r = f.subscribe(slots(8)).unwrap();
    assert!(control.unsubscribe(r.id()));
    assert_eq!(r.try_recv().unwrap_err(), RecvError::Closed);
    let [v, _] = Stream::new(1).frame();
    r.push_late(v);
    assert_eq!(r.try_recv().unwrap_err(), RecvError::Closed);
    assert_eq!(r.recv_timeout(Duration::from_secs(60)).unwrap_err(), RecvError::Closed);
}

#[test]
fn dropping_the_fanout_ends_every_subscription() {
    let f = Fanout::new();
    let control = f.control();
    let mut r = f.subscribe(slots(8)).unwrap();
    drop(f);
    assert!(control.is_ended());
    assert_eq!(r.recv_timeout(Duration::from_secs(60)).unwrap_err(), RecvError::Closed);
    let mut late = control.subscribe(slots(8)).unwrap();
    assert_eq!(late.try_recv().unwrap_err(), RecvError::Closed);
}

#[test]
fn a_parked_receiver_wakes_for_a_packet() {
    let mut f = Fanout::new();
    let mut r = f.subscribe(slots(8)).unwrap();
    let m = r.monitor();
    // The timeout is long enough that only the producer's wake returns the packet in a test run.
    let h = thread::spawn(move || r.recv_timeout(Duration::from_secs(3600)).map(|p| p.dts()));
    // Handshake: publish once the receiver has said it is going to park.
    while !m.is_waiting() {
        thread::yield_now();
    }
    Stream::new(1).publish(&mut f, 1);
    assert_eq!(h.join().unwrap(), Ok(0));
}

#[test]
fn recv_timeout_returns_when_the_time_is_up() {
    let f = Fanout::new();
    let mut r = f.subscribe(slots(8)).unwrap();
    assert_eq!(r.recv_timeout(Duration::ZERO).unwrap_err(), RecvError::Timeout);
}

#[test]
fn a_monitor_reads_the_counters_from_another_thread() {
    let mut f = Fanout::new();
    let mut r = f.subscribe(slots(8)).unwrap();
    let m = r.monitor();
    Stream::new(1).publish(&mut f, 1);
    drain(&mut r);
    let st = thread::spawn(move || m.stats()).join().unwrap();
    assert_eq!(st.delivered_packets, 2);
    assert_eq!(st.delivered_bytes, 14);
    assert_eq!(f.control().monitors().len(), 1);
}

#[test]
fn bad_ring_sizes_are_refused() {
    let f = Fanout::new();
    let ok = slots(1);
    assert_eq!(f.subscribe(RingConfig { slots: 0, ..ok }).unwrap_err(), ConfigError::NoSlots);
    assert_eq!(f.subscribe(RingConfig { bytes: 0, ..ok }).unwrap_err(), ConfigError::NoBytes);
    assert_eq!(f.subscribe(RingConfig { span: 0, ..ok }).unwrap_err(), ConfigError::NoSpan(0));
    assert_eq!(f.subscribe(RingConfig { span: -5, ..ok }).unwrap_err(), ConfigError::NoSpan(-5));
    assert_eq!(f.subscribe(RingConfig { slots: MAX_SLOTS + 1, ..ok }).unwrap_err(), ConfigError::TooManySlots(MAX_SLOTS + 1));
    assert!(f.subscribe(RingConfig { slots: MAX_SLOTS, ..ok }).is_ok());
    assert_eq!(f.subscribers(), 0);
}

#[test]
fn a_fanout_refuses_subscribers_past_the_limit() {
    let f = Fanout::new();
    let held: Vec<_> = (0..MAX_SUBSCRIBERS).map(|_| f.subscribe(slots(1)).unwrap()).collect();
    assert_eq!(f.control().subscribe(slots(1)).unwrap_err(), ConfigError::TooManySubscribers);
    assert_eq!(f.subscribers(), MAX_SUBSCRIBERS);
    drop(held);
    assert!(f.subscribe(slots(1)).is_ok());
}

#[test]
fn bad_pool_sizes_are_refused() {
    assert_eq!(PacketPool::new(0, 0).unwrap_err(), ConfigError::NoSlots);
    assert_eq!(PacketPool::new(MAX_SLOTS + 1, 0).unwrap_err(), ConfigError::TooManySlots(MAX_SLOTS + 1));
    assert_eq!(PacketPool::new(1, MAX_BUFFER_BYTES + 1).unwrap_err(), ConfigError::BufferTooLarge(MAX_BUFFER_BYTES + 1));
    assert_eq!(PacketPool::new(MAX_SLOTS, 0).map(|p| p.made()), Ok(MAX_SLOTS));
    assert_eq!(PacketPool::new(MAX_SLOTS, MAX_BUFFER_BYTES).unwrap_err(), ConfigError::PoolTooLarge { slots: MAX_SLOTS, bytes: MAX_BUFFER_BYTES });
    assert_eq!(PacketPool::new(16, MAX_BUFFER_BYTES).map(|p| p.made()), Ok(16));
    assert!(PacketPool::new(17, MAX_BUFFER_BYTES).is_err());
}

#[test]
fn for_stream_saturates_and_check_refuses_rates_too_large() {
    let rates =
        StreamRates { video_packets_per_second: u32::MAX, audio_packets_per_second: u32::MAX, bits_per_second: u64::MAX, ticks_per_second: i64::MAX };
    let cfg = RingConfig::for_stream(&rates, u32::MAX);
    assert_eq!(cfg.span, i64::MAX);
    assert!(cfg.bytes > 0);
    assert_eq!(cfg.check(), Err(ConfigError::TooManySlots(cfg.slots)));
}

#[test]
fn for_stream_sizes_three_seconds_of_a_6000_kbps_stream() {
    let rates = StreamRates { video_packets_per_second: 60, audio_packets_per_second: 47, bits_per_second: 6_160_000, ticks_per_second: 1000 };
    let cfg = RingConfig::for_stream(&rates, 3);
    assert_eq!(cfg, RingConfig { slots: 321, bytes: 3_465_000, span: 3000, video: true });
    let audio = StreamRates { video_packets_per_second: 0, ..rates };
    assert!(!RingConfig::for_stream(&audio, 3).video);
}

#[test]
fn for_stream_keeps_a_low_bit_rate() {
    let rates = StreamRates { video_packets_per_second: 0, audio_packets_per_second: 1, bits_per_second: 7, ticks_per_second: 1000 };
    let cfg = RingConfig::for_stream(&rates, 10);
    assert_eq!(cfg.bytes, 14);
    assert_eq!(cfg.check(), Ok(()));
}
