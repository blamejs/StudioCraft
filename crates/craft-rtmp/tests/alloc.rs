//! Chunking messages neither allocates nor frees memory once each chunk stream has sent its first
//! message. This file holds one test, so no other test in the same process allocates while it
//! counts.

use std::alloc::System;
use std::io;

use craft_rtmp::{ChunkWriter, MessageHeader, csid, msg_type};
use stats_alloc::{INSTRUMENTED_SYSTEM, Region, StatsAlloc};

#[global_allocator]
static GLOBAL: &StatsAlloc<System> = &INSTRUMENTED_SYSTEM;

#[test]
fn chunking_allocates_nothing_in_steady_state() {
    let video = vec![0x41u8; 12_500];
    let audio = vec![0x21u8; 426];
    let mut w = ChunkWriter::new();
    w.set_chunk_size(4096).unwrap();
    let send = |t: u32, w: &mut ChunkWriter| {
        let v = MessageHeader { csid: csid::VIDEO, type_id: msg_type::VIDEO, stream_id: 1, timestamp: t };
        w.write_message(&mut io::sink(), &v, &[&[0x27, 1, 0, 0, 0], &video]).unwrap();
        let a = MessageHeader { csid: csid::AUDIO, type_id: msg_type::AUDIO, stream_id: 1, timestamp: t };
        w.write_message(&mut io::sink(), &a, &[&[0xAF, 1], &audio]).unwrap();
    };
    // The first message on each chunk stream records its state.
    send(0, &mut w);
    let region = Region::new(GLOBAL);
    for f in 1..600u32 {
        // Past the 24-bit limit too, where the extended timestamp is written.
        let t = if f == 300 { 0x0100_0000 } else { f * 16 + if f > 300 { 0x0100_0000 } else { 0 } };
        send(t, &mut w);
    }
    let stats = region.change();
    assert_eq!((stats.allocations, stats.deallocations, stats.reallocations), (0, 0, 0), "{stats:?}");
}
