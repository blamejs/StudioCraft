use std::time::Duration;

use crate::{Config, Error, Event, JitterBuffer, LastSenderReport, Packet, Push, SLOTS};

const PAYLOAD: &[u8] = &[1, 2, 3];

fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

fn pkt(sequence: u16, timestamp: u32) -> Packet<'static> {
    Packet::new(96, sequence, timestamp, 7, false, PAYLOAD)
}

#[derive(Debug, PartialEq, Eq)]
enum Out {
    P(u16),
    Gap(u64),
}

fn drain(jb: &mut JitterBuffer, now: Duration) -> Vec<Out> {
    let mut out = Vec::new();
    while let Some(e) = jb.pop(now) {
        out.push(match e {
            Event::Packet(r) => Out::P(r.sequence),
            Event::Gap { missing } => Out::Gap(missing),
        });
    }
    out
}

fn buffer() -> JitterBuffer {
    JitterBuffer::new(Config::new(90_000)).unwrap()
}

#[test]
fn packets_are_released_in_order_after_the_delay() {
    let mut jb = buffer();
    for s in [1, 3, 2] {
        assert_eq!(jb.push(&pkt(s, 0), Duration::ZERO), Push::Stored);
    }
    assert_eq!(jb.next_wake(), Some(ms(60)));
    assert_eq!(drain(&mut jb, ms(59)), []);
    assert_eq!(drain(&mut jb, ms(60)), [Out::P(1), Out::P(2), Out::P(3)]);
    assert_eq!(jb.next_wake(), None);
    assert_eq!(jb.held(), 0);
    assert_eq!(jb.bytes(), 0);
    let s = jb.stats();
    assert_eq!((s.received, s.released, s.missing), (3, 3, 0));
}

#[test]
fn the_payload_and_fields_come_back() {
    let mut jb = buffer();
    let p = Packet::new(100, 9, 1234, 7, true, &[5, 6, 7, 8]);
    jb.push(&p, ms(1));
    let Some(Event::Packet(r)) = jb.pop(ms(100)) else { panic!("no packet") };
    assert_eq!((r.sequence, r.timestamp, r.marker, r.payload_type, r.arrival, r.payload), (9, 1234, true, 100, ms(1), &[5, 6, 7, 8][..]));
}

#[test]
fn sequence_numbers_wrap() {
    let mut jb = buffer();
    for s in [65534, 0, 65535, 1] {
        assert_eq!(jb.push(&pkt(s, 0), Duration::ZERO), Push::Stored);
    }
    assert_eq!(drain(&mut jb, ms(60)), [Out::P(65534), Out::P(65535), Out::P(0), Out::P(1)]);
    let r = jb.report_block(1, None, ms(60)).unwrap();
    assert_eq!(r.highest_sequence, 0x0001_0001);
    assert_eq!(r.cumulative_lost, 0);
}

#[test]
fn a_missing_packet_is_waited_for_then_given_up() {
    let mut jb = buffer();
    jb.push(&pkt(1, 0), Duration::ZERO);
    jb.push(&pkt(3, 0), ms(10));
    assert_eq!(drain(&mut jb, ms(60)), [Out::P(1)]);
    // Packet 2 waits until the delay has passed since packet 3 arrived.
    assert_eq!(jb.next_wake(), Some(ms(70)));
    assert_eq!(drain(&mut jb, ms(69)), []);
    assert_eq!(drain(&mut jb, ms(70)), [Out::Gap(1), Out::P(3)]);
    assert_eq!(jb.stats().missing, 1);
    // Packet 2 arriving now is late.
    assert_eq!(jb.push(&pkt(2, 0), ms(71)), Push::Late);
    assert_eq!(jb.stats().late, 1);
}

#[test]
fn a_missing_packet_is_waited_for_from_the_first_later_packet_to_arrive() {
    let mut jb = buffer();
    jb.push(&pkt(1, 0), Duration::ZERO);
    assert_eq!(drain(&mut jb, ms(60)), [Out::P(1)]);
    // Packet 4 arrives before packet 3, and packet 2 never does.
    jb.push(&pkt(4, 0), ms(61));
    jb.push(&pkt(3, 0), ms(100));
    assert_eq!(jb.next_wake(), Some(ms(121)));
    assert_eq!(drain(&mut jb, ms(120)), []);
    assert_eq!(drain(&mut jb, ms(121)), [Out::Gap(1)]);
    assert_eq!(drain(&mut jb, ms(161)), [Out::P(3), Out::P(4)]);
}

#[test]
fn next_wake_is_zero_once_a_packet_is_due() {
    let mut jb = buffer();
    jb.push(&pkt(1, 0), Duration::ZERO);
    assert_eq!(jb.next_wake(), Some(ms(60)));
    // The buffer's clock passes the packet's time without a pop.
    jb.push(&pkt(1, 0), ms(100));
    assert_eq!(jb.next_wake(), Some(Duration::ZERO));
}

#[test]
fn a_packet_that_fills_the_gap_in_time_is_released_in_order() {
    let mut jb = buffer();
    jb.push(&pkt(1, 0), Duration::ZERO);
    jb.push(&pkt(3, 0), ms(10));
    jb.push(&pkt(2, 0), ms(65));
    assert_eq!(drain(&mut jb, ms(70)), [Out::P(1)]);
    assert_eq!(drain(&mut jb, ms(125)), [Out::P(2), Out::P(3)]);
}

#[test]
fn each_missing_packet_is_listed_for_a_nack_once() {
    let mut jb = buffer();
    jb.push(&pkt(1, 0), Duration::ZERO);
    jb.push(&pkt(5, 0), ms(1));
    let mut lost = Vec::new();
    assert_eq!(jb.nacks(&mut lost, 2), 2);
    assert_eq!(lost, [2, 3]);
    assert_eq!(jb.nacks(&mut lost, 10), 1);
    assert_eq!(lost, [2, 3, 4]);
    assert_eq!(jb.nacks(&mut lost, 10), 0);
    jb.push(&pkt(3, 0), ms(2));
    jb.push(&pkt(7, 0), ms(3));
    lost.clear();
    jb.nacks(&mut lost, 10);
    assert_eq!(lost, [6]);
}

#[test]
fn duplicates_are_dropped() {
    let mut jb = buffer();
    assert_eq!(jb.push(&pkt(1, 0), Duration::ZERO), Push::Stored);
    assert_eq!(jb.push(&pkt(1, 0), ms(1)), Push::Duplicate);
    assert_eq!(drain(&mut jb, ms(100)), [Out::P(1)]);
    assert_eq!(jb.push(&pkt(1, 0), ms(101)), Push::Late);
    let s = jb.stats();
    assert_eq!((s.duplicates, s.late, s.released), (1, 1, 1));
}

#[test]
fn a_sender_that_starts_again_is_followed_after_two_packets_in_sequence() {
    let mut jb = buffer();
    jb.push(&pkt(1, 0), Duration::ZERO);
    jb.push(&pkt(2, 0), Duration::ZERO);
    assert_eq!(jb.push(&pkt(40_000, 0), ms(1)), Push::Stray);
    // A packet of the old sequence in between keeps the old sequence.
    assert_eq!(jb.push(&pkt(3, 0), ms(2)), Push::Stored);
    assert_eq!(jb.push(&pkt(40_000, 0), ms(3)), Push::Stray);
    assert_eq!(jb.push(&pkt(40_001, 0), ms(4)), Push::Restarted);
    assert_eq!(jb.push(&pkt(40_002, 0), ms(4)), Push::Stored);
    assert_eq!(drain(&mut jb, ms(100)), [Out::Gap(3), Out::P(40_001), Out::P(40_002)]);
    let s = jb.stats();
    assert_eq!((s.stray, s.restarts, s.dropped), (2, 1, 3));
    // The report counts from the new start.
    let r = jb.report_block(1, None, ms(100)).unwrap();
    assert_eq!((r.highest_sequence, r.cumulative_lost), (40_002, 0));
}

#[test]
fn a_packet_far_behind_is_stray() {
    let mut jb = buffer();
    jb.push(&pkt(1000, 0), Duration::ZERO);
    // 100 behind the newest is a stray; 99 behind is a reordered packet, late here.
    assert_eq!(jb.push(&pkt(900, 0), ms(1)), Push::Stray);
    assert_eq!(jb.push(&pkt(901, 0), ms(1)), Push::Late);
    // Two strays in sequence behind are a sender that started again.
    assert_eq!(jb.push(&pkt(500, 0), ms(1)), Push::Stray);
    assert_eq!(jb.push(&pkt(501, 0), ms(1)), Push::Restarted);
}

#[test]
fn the_byte_limit_drops_the_oldest_packets() {
    let mut jb = JitterBuffer::new(Config { max_bytes: 1000, ..Config::new(90_000) }).unwrap();
    let big = [0u8; 400];
    for s in 1..=3 {
        jb.push(&Packet::new(96, s, 0, 7, false, &big), Duration::ZERO);
    }
    assert_eq!(jb.held(), 2);
    assert_eq!(jb.bytes(), 800);
    assert_eq!(jb.stats().dropped, 1);
    assert_eq!(drain(&mut jb, ms(60)), [Out::Gap(1), Out::P(2), Out::P(3)]);
}

#[test]
fn the_byte_limit_holds_for_first_and_restart_packets() {
    let mut jb = JitterBuffer::new(Config { max_bytes: 1000, ..Config::new(90_000) }).unwrap();
    let big = [0u8; 600];
    assert_eq!(jb.push(&Packet::new(96, 1, 0, 7, false, &big), Duration::ZERO), Push::Stored);
    assert_eq!((jb.held(), jb.bytes(), jb.stats().dropped), (1, 600, 0));
    // Two packets in sequence far from the last make the buffer start again.
    assert_eq!(jb.push(&Packet::new(96, 30_000, 0, 7, false, &big), ms(1)), Push::Stray);
    assert_eq!(jb.push(&Packet::new(96, 30_001, 0, 7, false, &big), ms(2)), Push::Restarted);
    assert_eq!((jb.held(), jb.bytes(), jb.stats().dropped), (1, 600, 1));
    assert_eq!(jb.push(&Packet::new(96, 30_002, 0, 7, false, &big), ms(3)), Push::Stored);
    assert_eq!((jb.held(), jb.bytes(), jb.stats().dropped), (1, 600, 2));
}

#[test]
fn a_payload_over_the_byte_limit_is_dropped_before_it_is_copied() {
    let mut jb = JitterBuffer::new(Config { max_bytes: 1000, ..Config::new(90_000) }).unwrap();
    let huge = vec![0u8; 64 * 1024 * 1024];
    assert_eq!(jb.push(&Packet::new(96, 4, 0, 7, false, &huge), Duration::ZERO), Push::Oversized);
    assert_eq!((jb.held(), jb.bytes()), (1, 0));
    assert_eq!(jb.push(&pkt(5, 0), ms(1)), Push::Stored);
    // The packets already held stay.
    assert_eq!(jb.push(&Packet::new(96, 6, 0, 7, false, &huge), ms(2)), Push::Oversized);
    assert_eq!((jb.held(), jb.bytes()), (3, PAYLOAD.len()));
    assert_eq!(jb.push(&pkt(7, 0), ms(3)), Push::Stored);
    let mut nacks = Vec::new();
    assert_eq!(jb.nacks(&mut nacks, 10), 0);
    assert_eq!(drain(&mut jb, ms(100)), [Out::Gap(1), Out::P(5), Out::Gap(1), Out::P(7)]);
    let stats = jb.stats();
    assert_eq!((stats.dropped, stats.missing, stats.released), (2, 0, 2));
    // The oversized packets arrived, so the report counts none lost.
    let block = jb.report_block(1, None, ms(100)).unwrap();
    assert_eq!((block.cumulative_lost, block.fraction_lost, block.highest_sequence), (0, 0, 7));
    assert!(jb.spare_capacity() < 1000, "{}", jb.spare_capacity());
}

#[test]
fn an_oversized_packet_in_sequence_cancels_a_pending_restart() {
    let mut jb = JitterBuffer::new(Config { max_bytes: 1000, ..Config::new(90_000) }).unwrap();
    let huge = [0u8; 1400];
    assert_eq!(jb.push(&pkt(1, 0), Duration::ZERO), Push::Stored);
    assert_eq!(jb.push(&pkt(40_000, 0), ms(1)), Push::Stray);
    assert_eq!(jb.push(&Packet::new(96, 2, 0, 7, false, &huge), ms(2)), Push::Oversized);
    assert_eq!(jb.push(&pkt(40_001, 0), ms(3)), Push::Stray);
    assert_eq!(jb.stats().restarts, 0);
}

#[test]
fn spare_buffers_together_hold_no_more_than_the_byte_limit() {
    let mut jb = JitterBuffer::new(Config { max_bytes: 1000, ..Config::new(90_000) }).unwrap();
    let small = [0u8; 100];
    let full = [0u8; 1000];
    // Ten 100-byte packets leave nine buffers to reuse.
    for s in 1..=10 {
        assert_eq!(jb.push(&Packet::new(96, s, 0, 7, false, &small), Duration::ZERO), Push::Stored);
    }
    assert_eq!(drain(&mut jb, ms(100)).len(), 10);
    assert!(jb.spare_capacity() > 0);
    // Each 1000-byte packet grows a reused buffer to the whole limit; keeping them all would
    // hold more than the limit.
    for (s, t) in [(11, 200), (12, 300), (13, 400)] {
        assert_eq!(jb.push(&Packet::new(96, s, 0, 7, false, &full), ms(t)), Push::Stored);
        assert_eq!(drain(&mut jb, ms(t + 100)), [Out::P(s)]);
        assert!(jb.spare_capacity() <= 1000, "{}", jb.spare_capacity());
    }
}

#[test]
fn held_duplicates_count_as_received_in_the_report() {
    let mut jb = buffer();
    for s in [1, 1, 3] {
        jb.push(&pkt(s, 0), Duration::ZERO);
    }
    let r = jb.report_block(9, None, ms(1)).unwrap();
    assert_eq!((r.cumulative_lost, r.fraction_lost), (0, 0));
}

#[test]
fn transit_differences_wrap() {
    // A 1 kHz clock: the same timestamp 1 ms apart is a one-tick change across the 32-bit wrap
    // of the transit time.
    let mut jb = JitterBuffer::new(Config::new(1000)).unwrap();
    jb.push(&pkt(1, 0x8000_0001), Duration::ZERO);
    jb.push(&pkt(2, 0x8000_0001), ms(1));
    assert_eq!(jb.stats().jitter, 0);
}

#[test]
fn the_span_limit_drops_packets_nobody_took() {
    let mut jb = JitterBuffer::new(Config { max_span: ms(100), ..Config::new(90_000) }).unwrap();
    jb.push(&pkt(1, 0), Duration::ZERO);
    jb.push(&pkt(2, 0), ms(50));
    jb.push(&pkt(3, 0), ms(130));
    assert_eq!(jb.held(), 2);
    assert_eq!(drain(&mut jb, ms(200)), [Out::Gap(1), Out::P(2), Out::P(3)]);
}

#[test]
fn a_packet_more_than_the_slots_ahead_gives_up_on_the_oldest() {
    let mut jb = buffer();
    jb.push(&pkt(0, 0), Duration::ZERO);
    jb.push(&pkt(2999, 0), Duration::ZERO);
    jb.push(&pkt(5998, 0), Duration::ZERO);
    assert_eq!(jb.held(), 2);
    assert_eq!(jb.stats().dropped, 1);
    // The buffer gave up on 0 to 1902 to make room, and waits for 1903 onward.
    let first = 5998 - SLOTS as u64 + 1;
    assert_eq!(drain(&mut jb, ms(60)), [Out::Gap(first), Out::Gap(2999 - first), Out::P(2999), Out::Gap(5998 - 2999 - 1), Out::P(5998)]);
}

#[test]
fn the_jitter_estimate_follows_rfc_3550_section_6_4_1() {
    // A 1 kHz clock, so one timestamp unit is 1 ms: packets every 20 ms that arrive 0 or 5 ms
    // late in turn.
    let mut jb = JitterBuffer::new(Config::new(1000)).unwrap();
    let mut j = 0.0f64;
    for i in 0..40u16 {
        let late = if i % 2 == 0 { 0 } else { 5 };
        jb.push(&pkt(i, u32::from(i) * 20), ms(u64::from(i) * 20 + late));
        if i > 0 {
            j += (5.0 - j) / 16.0;
        }
        // Section A.8's integer form rounds each step and reports the whole units, so it may
        // trail the exact value by up to about 1.5 units.
        let got = f64::from(jb.stats().jitter);
        assert!(got <= j + 0.5 && j - got < 1.5, "packet {i}: {got} against {j}");
    }
    // 16 measured packets on, the delay is four times the estimate, inside 40 to 150 ms.
    assert_eq!(jb.delay(), ms(40));
}

#[test]
fn the_delay_grows_with_the_jitter_up_to_its_limit() {
    // Packets every 100 ms that arrive 0 or 30 ms late in turn, then 0 or 90 ms late.
    let mut jb = JitterBuffer::new(Config::new(1000)).unwrap();
    for i in 0..17u16 {
        let late = if i % 2 == 0 { 0 } else { 30 };
        jb.push(&pkt(i, u32::from(i) * 100), ms(u64::from(i) * 100 + late));
        if i < 16 {
            assert_eq!(jb.delay(), ms(60), "packet {i}");
        }
        while jb.pop(ms(u64::from(i) * 100 + late)).is_some() {}
    }
    let j = jb.stats().jitter;
    assert!((15..30).contains(&j), "{j}");
    assert_eq!(jb.delay(), ms(u64::from(j) * 4));
    for i in 17..200u16 {
        let late = if i % 2 == 0 { 0 } else { 90 };
        jb.push(&pkt(i, u32::from(i) * 100), ms(u64::from(i) * 100 + late));
        while jb.pop(ms(u64::from(i) * 100 + late)).is_some() {}
    }
    assert_eq!(jb.delay(), ms(150));
}

#[test]
fn the_report_block_counts_loss_since_the_last_report() {
    let mut jb = buffer();
    assert_eq!(jb.report_block(9, None, Duration::ZERO), None);
    for s in [1, 2, 4, 5] {
        jb.push(&pkt(s, 0), Duration::ZERO);
    }
    let sr = LastSenderReport { ntp: 0x1122_3344_5566_7788, received_at: ms(1000) };
    let r = jb.report_block(9, Some(sr), ms(1500)).unwrap();
    assert_eq!(r.ssrc, 9);
    assert_eq!(r.highest_sequence, 5);
    assert_eq!(r.cumulative_lost, 1);
    assert_eq!(r.fraction_lost, 51);
    assert_eq!(r.last_sr, 0x3344_5566);
    assert_eq!(r.delay_since_last_sr, 32_768);
    let again = jb.report_block(9, None, ms(1500)).unwrap();
    assert_eq!((again.fraction_lost, again.cumulative_lost, again.last_sr, again.delay_since_last_sr), (0, 1, 0, 0));
    // A late packet counts as received.
    drain(&mut jb, ms(2000));
    jb.push(&pkt(3, 0), ms(2001));
    assert_eq!(jb.report_block(9, None, ms(2001)).unwrap().cumulative_lost, 0);
}

#[test]
fn a_clock_that_goes_back_is_held_at_its_latest_time() {
    let mut jb = buffer();
    jb.push(&pkt(1, 0), ms(100));
    jb.push(&pkt(2, 0), ms(50));
    assert_eq!(drain(&mut jb, ms(159)), []);
    assert_eq!(drain(&mut jb, ms(160)), [Out::P(1), Out::P(2)]);
}

#[test]
fn bad_settings_are_refused() {
    assert!(matches!(JitterBuffer::new(Config::new(0)), Err(Error::BadInput(_))));
    assert!(matches!(JitterBuffer::new(Config { min_delay: ms(200), ..Config::new(90_000) }), Err(Error::BadInput(_))));
    assert!(matches!(JitterBuffer::new(Config { max_bytes: 0, ..Config::new(90_000) }), Err(Error::BadInput(_))));
    assert!(matches!(JitterBuffer::new(Config { max_span: Duration::ZERO, ..Config::new(90_000) }), Err(Error::BadInput(_))));
    let jb = JitterBuffer::new(Config { start_delay: ms(500), ..Config::new(90_000) }).unwrap();
    assert_eq!(jb.delay(), ms(150));
}
