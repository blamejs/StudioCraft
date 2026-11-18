use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use super::*;

/// A ring with room for `slots` packets and plenty of bytes and span.
fn slots(slots: usize) -> RingConfig {
    RingConfig { slots, bytes: MAX_RING_BYTES, span: i64::MAX, video: true, wake_every: 1 }
}

/// A ring of `slots` packets and `bytes` bytes, with the other fields as in [`slots`].
fn sized(slots: usize, bytes: usize) -> RingConfig {
    RingConfig { bytes, ..self::slots(slots) }
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
    let mut next = vec![1u8; 4096];
    let q = pool.share(&mut next, Info::video(1, 1, false)).unwrap();
    assert_eq!(next.as_ptr(), at, "the free packet's old buffer comes back");
    assert!(next.is_empty() && next.capacity() >= 4096);
    assert_eq!(q.data(), [1u8; 4096]);
}

#[test]
fn share_hands_the_encoder_back_a_buffer_of_its_size() {
    // Video swapped in with `share` and audio copied with `share_copy` share one pool.
    let mut pool = PacketPool::new(4, 0).unwrap();
    let mut video = vec![0u8; 12_500];
    for i in 0..50 {
        video.resize(12_500, 1);
        let v = pool.share(&mut video, Info::video(i, i, false)).unwrap();
        let a = pool.share_copy(&[2; 426], Info::audio(i)).unwrap();
        assert!(a.capacity() <= 4 * 426, "an audio packet keeps a buffer of {} bytes", a.capacity());
        drop((v, a));
        if i > 0 {
            assert!(video.capacity() >= 12_500, "frame {i}: the encoder got back a buffer of {} bytes", video.capacity());
        }
    }
}

#[test]
fn a_pool_of_empty_packets_takes_a_packet_of_the_largest_size() {
    // Every packet the pool can make exists and has no buffer.
    let mut pool = PacketPool::new(MAX_SLOTS, 0).unwrap();
    let p = pool.share_copy(&vec![5; MAX_BUFFER_BYTES], Info::video(0, 0, true)).unwrap();
    assert_eq!(p.len(), MAX_BUFFER_BYTES);
    assert_eq!(pool.made(), MAX_SLOTS);
}

#[test]
fn a_packet_keeps_no_buffer_much_larger_than_its_bytes() {
    let mut pool = PacketPool::new(1, 0).unwrap();
    drop(pool.share_copy(&vec![1; 400_000], Info::video(0, 0, true)).unwrap());
    for i in 0..100 {
        let a = pool.share_copy(&[2; 426], Info::audio(i)).unwrap();
        assert!(a.capacity() <= 4 * 426, "an audio packet keeps a buffer of {} bytes", a.capacity());
    }
    // An encoder buffer far larger than its packet is copied from, and the encoder keeps it.
    let mut enc = Vec::with_capacity(3 << 20);
    enc.extend_from_slice(&[3; 20_000]);
    let at = enc.as_ptr();
    let v = pool.share(&mut enc, Info::video(1, 1, false)).unwrap();
    assert!(v.capacity() <= 4 * 20_000, "a 20 000-byte packet keeps a buffer of {} bytes", v.capacity());
    assert_eq!(v.data(), [3; 20_000]);
    assert_eq!(enc.as_ptr(), at);
    assert!(enc.is_empty() && enc.capacity() == 3 << 20);
    assert!(pool.held_bytes() < 1 << 20, "the pool holds {} bytes", pool.held_bytes());
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
fn share_copy_finds_a_fitting_free_buffer_behind_smaller_ones() {
    let mut pool = PacketPool::new(1, 0).unwrap();
    // One free 1000-byte buffer behind nine free 600-byte buffers, all in the 512 to 1023 list.
    let mut fitting = vec![0u8; 1000];
    let fit = pool.share(&mut fitting, Info::data(0)).unwrap();
    let small: Vec<_> = (1..10)
        .map(|i| {
            let mut buf = vec![0u8; 600];
            pool.share(&mut buf, Info::data(i)).unwrap()
        })
        .collect();
    drop(fit);
    drop(small);
    let (made, held) = (pool.made(), pool.held_bytes());
    let p = pool.share_copy(&[1; 1000], Info::data(10)).unwrap();
    assert_eq!(p.data(), [1; 1000]);
    assert_eq!((pool.made(), pool.held_bytes()), (made, held), "the pool used the free buffer instead of allocating");
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
            let mut buf = vec![0u8; per];
            pool.share(&mut buf, Info::data(i)).unwrap()
        })
        .collect();
    assert_eq!(pool.held_bytes(), MAX_POOL_BYTES);
    let mut extra = vec![0u8; per];
    assert_eq!(pool.share(&mut extra, Info::data(16)).unwrap_err(), ShareError::PoolFull);
    assert_eq!(extra.capacity(), per);
    assert_eq!(pool.share_copy(&[1], Info::data(16)).unwrap_err(), ShareError::PoolFull);
    drop(held);
    assert!(pool.share_copy(&[1], Info::data(16)).is_ok());
}

#[test]
fn a_full_pool_releases_free_buffers_to_make_room() {
    let mut pool = PacketPool::new(1, 0).unwrap();
    let held: Vec<_> = (0..16)
        .map(|i| {
            let mut big = vec![0u8; MAX_BUFFER_BYTES];
            pool.share(&mut big, Info::data(i)).unwrap()
        })
        .collect();
    assert_eq!(pool.held_bytes(), MAX_POOL_BYTES);
    drop(held);
    // No free buffer is of a 4-byte packet's size, and a new one would pass the limit, so the
    // pool releases one large free buffer.
    let p = pool.share_copy(&[7; 4], Info::data(16)).unwrap();
    assert_eq!(p.data(), [7; 4]);
    assert_eq!(pool.held_bytes(), MAX_POOL_BYTES - MAX_BUFFER_BYTES + 64);
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
    let mut r = f.subscribe(sized(100, 30)).unwrap();
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
    let mut r = f.subscribe(sized(4, 8)).unwrap();
    let mut pool = PacketPool::new(1, 0).unwrap();
    let big = pool.share_copy(&[0; 100], Info::video(0, 0, true)).unwrap();
    assert_eq!(f.publish(&big), 1);
    assert_eq!(drain(&mut r).len(), 1);
}

#[test]
fn the_receiver_skips_stale_packets_to_the_next_keyframe() {
    let mut f = Fanout::new();
    let mut r = f.subscribe(RingConfig { span: 100, ..sized(1000, 1 << 20) }).unwrap();
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
    let mut r = f.subscribe(RingConfig { video: false, ..sized(2, 1 << 20) }).unwrap();
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
fn a_removed_subscriber_delivers_its_queued_packets_while_others_go_on() {
    let mut f = Fanout::new();
    let control = f.control();
    let mut gone = f.subscribe(RingConfig { span: 100, ..sized(64, 1 << 20) }).unwrap();
    let _stays = f.subscribe(slots(64)).unwrap();
    let mut s = Stream::new(5);
    s.publish(&mut f, 3);
    assert!(control.unsubscribe(gone.id()));
    // The stream goes on far past the removed ring's span.
    s.publish(&mut f, 30);
    assert_eq!(drain(&mut gone).len(), 6, "the packets queued before the removal are delivered");
    assert_eq!(gone.stats().skipped_packets, 0);
}

#[test]
fn no_packet_follows_closed() {
    let f = Fanout::new();
    let control = f.control();
    let mut r = f.subscribe(slots(8)).unwrap();
    assert!(control.unsubscribe(r.id()));
    assert_eq!(r.try_recv().unwrap_err(), RecvError::Closed);
    let mut pool = PacketPool::new(1, 64).unwrap();
    r.push_late(pool.share_copy(&[1; 10], Info::video(0, 0, true)).unwrap());
    assert_eq!(pool.free(), 0);
    assert_eq!(r.try_recv().unwrap_err(), RecvError::Closed);
    assert_eq!(pool.free(), 1, "the late packet goes back to the pool while the receiver lives");
    let st = r.stats();
    assert_eq!((st.queued_packets, st.discarded_packets, st.discarded_bytes), (0, 1, 10));
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
fn a_batching_receiver_parks_at_most_one_batch_wait() {
    let hour = Duration::from_secs(3600);
    // The producer does not wake a receiver for fewer than `wake_every` packets, so one that
    // batches looks at its ring again after `BATCH_WAIT` to take a partial batch.
    assert_eq!(ring::park_for(&RingConfig { wake_every: 3, ..slots(8) }, hour), BATCH_WAIT);
    assert_eq!(ring::park_for(&RingConfig { wake_every: 3, ..slots(8) }, Duration::from_millis(5)), Duration::from_millis(5));
    assert_eq!(ring::park_for(&slots(8), hour), hour, "a receiver woken for each packet parks for its whole timeout");
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
    assert_eq!(f.subscribe(RingConfig { slots: MAX_RING_SLOTS + 1, ..ok }).unwrap_err(), ConfigError::TooManyRingSlots(MAX_RING_SLOTS + 1));
    assert!(f.subscribe(RingConfig { slots: MAX_RING_SLOTS, ..ok }).is_ok());
    assert_eq!(f.subscribe(RingConfig { bytes: MAX_RING_BYTES + 1, ..ok }).unwrap_err(), ConfigError::TooManyRingBytes(MAX_RING_BYTES + 1));
    assert_eq!(f.subscribe(RingConfig { wake_every: 0, ..ok }).unwrap_err(), ConfigError::BadWake(0));
    assert_eq!(f.subscribe(RingConfig { wake_every: 2, ..ok }).unwrap_err(), ConfigError::BadWake(2));
    assert_eq!(f.subscribers(), 0);
}

#[test]
fn a_ring_the_pool_cannot_back_is_refused() {
    // Ten minutes of a 6000 kb/s stream: more packets than one ring may hold.
    let rates = StreamRates { video_packets_per_second: 60, audio_packets_per_second: 47, bits_per_second: 6_000_000, ticks_per_second: 1000 };
    let cfg = RingConfig::for_stream(&rates, 600);
    assert_eq!((cfg.slots, cfg.bytes), (64_200, 675_000_000));
    assert_eq!(cfg.check(), Err(ConfigError::TooManyRingSlots(64_200)));
    assert_eq!(RingConfig { slots: 1000, ..cfg }.check(), Err(ConfigError::TooManyRingBytes(675_000_000)));
    assert_eq!(Fanout::new().subscribe(cfg).unwrap_err(), ConfigError::TooManyRingSlots(64_200));
}

#[test]
fn a_stalled_subscriber_never_starves_the_pool_for_the_others() {
    let mut f = Fanout::new();
    // Two stalled rings that together hold every packet the pool can make: the second subscribes
    // once the first is full, so they hold different packets.
    let mut stalled = vec![f.subscribe(slots(MAX_RING_SLOTS)).unwrap()];
    let mut healthy = f.subscribe(slots(8)).unwrap();
    let mut pool = PacketPool::new(1, 0).unwrap();
    let total = MAX_SLOTS + 1000;
    let mut got = 0;
    for i in 0..total as i64 {
        if i == MAX_RING_SLOTS as i64 {
            stalled.push(f.subscribe(slots(MAX_RING_SLOTS)).unwrap());
        }
        let p = f.share_copy(&mut pool, &[], Info::video(i, i, true)).unwrap();
        f.publish(&p);
        drop(p);
        while healthy.try_recv().is_ok() {
            got += 1;
        }
    }
    assert_eq!(got, total, "the healthy subscriber got every packet");
    assert!(stalled.iter().any(|r| r.stats().discarded_packets > 0), "a stalled ring was emptied");
    assert_eq!(healthy.stats().discarded_packets, 0);
    assert!(pool.made() <= MAX_SLOTS);
}

#[test]
fn a_full_pool_never_empties_a_ring_of_another_pools_packets() {
    let mut f = Fanout::new();
    let stalled = f.subscribe(slots(8)).unwrap();
    let mut other = PacketPool::new(1, 0).unwrap();
    f.publish(&other.share_copy(&[1; 10], Info::video(0, 0, true)).unwrap());
    let mut full = PacketPool::new(MAX_SLOTS, 0).unwrap();
    let held: Vec<_> = (0..MAX_SLOTS).map(|i| full.share_copy(&[], Info::data(i as i64)).unwrap()).collect();
    assert_eq!(f.share_copy(&mut full, &[2], Info::data(0)).unwrap_err(), ShareError::PoolFull);
    let st = stalled.stats();
    assert_eq!((st.queued_packets, st.discarded_packets), (1, 0), "the other pool's packet stays queued");
    drop(held);
    assert!(f.share_copy(&mut full, &[2], Info::data(0)).is_ok());
}

#[test]
fn relief_stops_when_emptying_a_ring_frees_nothing_in_the_pool() {
    let mut f = Fanout::new();
    let stalled = f.subscribe(slots(8)).unwrap();
    let keeping_up = f.subscribe(slots(64)).unwrap();
    let also_keeping_up = f.subscribe(slots(64)).unwrap();
    let mut pool = PacketPool::new(MAX_SLOTS, 0).unwrap();
    // The caller keeps a clone of every packet, so emptying a ring returns none to the pool.
    let held: Vec<_> = (0..MAX_SLOTS).map(|i| pool.share_copy(&[1], Info::video(i as i64, i as i64, true)).unwrap()).collect();
    for p in held.iter().take(8) {
        f.publish(p);
    }
    for p in held.iter().skip(8).take(2) {
        assert!(also_keeping_up.offer_only_here(p));
    }
    assert_eq!(f.share_copy(&mut pool, &[2], Info::data(0)).unwrap_err(), ShareError::PoolFull);
    assert_eq!(stalled.stats().discarded_packets, 8, "the full ring was emptied");
    assert_eq!(also_keeping_up.stats().discarded_packets, 10, "then the fullest ring that keeps up");
    assert_eq!(keeping_up.stats().queued_packets, 8, "no further ring is emptied once a flush frees nothing");
    drop(held);
}

#[test]
fn relief_goes_on_while_another_stalled_ring_holds_the_same_packets() {
    let mut f = Fanout::new();
    let a = f.subscribe(slots(8)).unwrap();
    let b = f.subscribe(slots(8)).unwrap();
    let mut pool = PacketPool::new(MAX_SLOTS, 0).unwrap();
    let mut held: Vec<_> = (0..MAX_SLOTS).map(|i| pool.share_copy(&[1], Info::video(i as i64, i as i64, true)).unwrap()).collect();
    // Both stalled rings hold clones of the first 8 packets; the caller holds the rest.
    for p in held.drain(..8) {
        f.publish(&p);
    }
    let p = f.share_copy(&mut pool, &[2], Info::data(0)).unwrap();
    assert_eq!(p.data(), [2]);
    assert_eq!((a.stats().discarded_packets, b.stats().discarded_packets), (8, 8));
    drop(held);
}

#[test]
fn the_producer_wakes_a_parked_receiver_after_wake_every_packets() {
    let mut f = Fanout::new();
    let r = f.subscribe(RingConfig { wake_every: 3, ..slots(8) }).unwrap();
    let m = r.monitor();
    let mut s = Stream::new(1);
    m.set_waiting();
    let [v, a] = s.frame();
    f.publish(&v);
    f.publish(&a);
    assert_eq!(m.stats().wakes, 0);
    assert!(m.is_waiting());
    let [v, _] = s.frame();
    f.publish(&v);
    assert_eq!(m.stats().wakes, 1);
    assert!(!m.is_waiting());
}

#[test]
fn a_ring_full_by_bytes_wakes_its_parked_receiver() {
    let mut f = Fanout::new();
    // 14 bytes a frame and room for 20: the second frame's video packet overflows the ring.
    let r = f.subscribe(RingConfig { wake_every: 4, ..sized(8, 20) }).unwrap();
    let m = r.monitor();
    let mut s = Stream::new(1);
    m.set_waiting();
    s.publish(&mut f, 1);
    assert_eq!(m.stats().wakes, 0);
    let [v, _] = s.frame();
    f.publish(&v);
    assert_eq!(m.stats().wakes, 1);
}

#[test]
fn counters_never_show_more_delivered_than_accepted() {
    const FRAMES: usize = 50_000;
    let mut f = Fanout::new();
    let mut r = f.subscribe(slots(64)).unwrap();
    let m = r.monitor();
    let reader = thread::spawn(move || while r.recv_timeout(Duration::from_secs(60)).is_ok() {});
    let done = Arc::new(AtomicBool::new(false));
    let watching = Arc::clone(&done);
    let watcher = thread::spawn(move || {
        let mut bad = 0u64;
        let mut reads = 0u64;
        while !watching.load(Ordering::Acquire) {
            let st = m.stats();
            reads += 1;
            if st.delivered_packets + st.skipped_packets + st.discarded_packets > st.accepted_packets
                || st.delivered_bytes + st.skipped_bytes + st.discarded_bytes > st.accepted_bytes
            {
                bad += 1;
            }
        }
        (bad, reads)
    });
    let mut s = Stream::new(30);
    s.publish(&mut f, FRAMES);
    drop(f);
    reader.join().unwrap();
    done.store(true, Ordering::Release);
    let (bad, reads) = watcher.join().unwrap();
    assert!(reads > 0);
    assert_eq!(bad, 0, "{bad} of {reads} reads showed more delivered than accepted");
}

#[test]
fn a_removed_ring_is_freed_off_the_producer_thread() {
    let f = Fanout::new();
    let control = f.control();
    let r = f.subscribe(slots(8)).unwrap();
    let ring = r.ring_weak();
    // The producer is reading the list, as `publish` does, when the receiver is dropped.
    let held = f.hold_list();
    drop(r);
    drop(held);
    assert!(ring.upgrade().is_some(), "the producer's read of the list freed the ring");
    // The next change to the list, on a thread that is not the producer's, frees it.
    let _other = control.subscribe(slots(8)).unwrap();
    assert!(ring.upgrade().is_none());
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
    assert_eq!(cfg.check(), Err(ConfigError::TooManyRingSlots(cfg.slots)));
}

#[test]
fn for_stream_sizes_three_seconds_of_a_6000_kbps_stream() {
    let rates = StreamRates { video_packets_per_second: 60, audio_packets_per_second: 47, bits_per_second: 6_160_000, ticks_per_second: 1000 };
    let cfg = RingConfig::for_stream(&rates, 3);
    assert_eq!(cfg, RingConfig { slots: 321, bytes: 3_465_000, span: 3000, video: true, wake_every: 3 });
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
