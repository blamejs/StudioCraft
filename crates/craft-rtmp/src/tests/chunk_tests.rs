use std::io::{self, Write};

use proptest::prelude::*;

use crate::Error;
use crate::chunk::{ChunkReader, ChunkWriter, MAX_MESSAGE, Message, MessageHeader, msg_type};

fn header(csid: u32, type_id: u8, stream_id: u32, timestamp: u32) -> MessageHeader {
    MessageHeader { csid, type_id, stream_id, timestamp }
}

fn write(w: &mut ChunkWriter, h: MessageHeader, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let n = w.write_message(&mut out, &h, &[payload]).unwrap();
    assert_eq!(n, out.len());
    out
}

#[test]
fn audio_messages_compress_as_example_1() {
    // §5.3.2.1: four 32-byte audio messages 20 ms apart on message stream 12345.
    let mut w = ChunkWriter::new();
    let payload = [0xA5; 32];
    let chunks: Vec<_> = [1000, 1020, 1040, 1060].iter().map(|&t| write(&mut w, header(3, 8, 12345, t), &payload)).collect();
    assert_eq!(chunks.iter().map(Vec::len).collect::<Vec<_>>(), [44, 36, 33, 33]);
    assert_eq!(chunks[0][..12], [0x03, 0x00, 0x03, 0xE8, 0x00, 0x00, 0x20, 0x08, 0x39, 0x30, 0x00, 0x00]);
    assert_eq!(chunks[1][..4], [0x83, 0x00, 0x00, 0x14]);
    assert_eq!(chunks[2][0], 0xC3);
    assert_eq!(chunks[3][0], 0xC3);
    assert!(chunks.iter().all(|c| c.ends_with(&payload)));
}

#[test]
fn a_long_message_splits_as_example_2() {
    // §5.3.2.2: a 307-byte video message at the 128-byte chunk size.
    let mut w = ChunkWriter::new();
    let payload: Vec<u8> = (0..307).map(|i| i as u8).collect();
    let out = write(&mut w, header(4, 9, 12346, 1000), &payload);
    assert_eq!(out.len(), 140 + 129 + 52);
    assert_eq!(out[..12], [0x04, 0x00, 0x03, 0xE8, 0x00, 0x01, 0x33, 0x09, 0x3A, 0x30, 0x00, 0x00]);
    assert_eq!(out[12..140], payload[..128]);
    assert_eq!(out[140], 0xC4);
    assert_eq!(out[141..269], payload[128..256]);
    assert_eq!(out[269], 0xC4);
    assert_eq!(out[270..], payload[256..]);
}

#[test]
fn each_change_picks_the_smallest_header() {
    let mut w = ChunkWriter::new();
    let fmt = |c: &[u8]| c[0] >> 6;
    assert_eq!(fmt(&write(&mut w, header(6, 9, 1, 20), &[1; 10])), 0);
    // The delta after a type 0 header is new, so type 3 never starts this message.
    assert_eq!(fmt(&write(&mut w, header(6, 9, 1, 40), &[1; 10])), 2);
    assert_eq!(fmt(&write(&mut w, header(6, 9, 1, 60), &[1; 10])), 3);
    assert_eq!(fmt(&write(&mut w, header(6, 9, 1, 70), &[1; 10])), 2);
    assert_eq!(fmt(&write(&mut w, header(6, 9, 1, 80), &[1; 11])), 1);
    assert_eq!(fmt(&write(&mut w, header(6, 8, 1, 90), &[1; 11])), 1);
    // A timestamp that goes back, and a new message stream, need type 0.
    assert_eq!(fmt(&write(&mut w, header(6, 8, 1, 50), &[1; 11])), 0);
    assert_eq!(fmt(&write(&mut w, header(6, 8, 2, 60), &[1; 11])), 0);
    // Another chunk stream starts with type 0 too.
    assert_eq!(fmt(&write(&mut w, header(4, 8, 2, 60), &[1; 11])), 0);
}

#[test]
fn large_timestamps_use_the_extended_field_in_every_chunk() {
    let mut w = ChunkWriter::new();
    let payload = [7u8; 200];
    let out = write(&mut w, header(6, 9, 1, 0x0100_0000), &payload);
    // Type 0 with 0xFFFFFF, the extended timestamp after the header, and again after the
    // continuation header.
    assert_eq!(out[1..4], [0xFF, 0xFF, 0xFF]);
    assert_eq!(out[12..16], [0x01, 0x00, 0x00, 0x00]);
    assert_eq!(out[16 + 128], 0xC6);
    assert_eq!(out[16 + 128 + 1..16 + 128 + 5], [0x01, 0x00, 0x00, 0x00]);
    assert_eq!(out.len(), 16 + 128 + 5 + 72);
    // A small delta after it needs no extended field.
    let next = write(&mut w, header(6, 9, 1, 0x0100_0010), &payload);
    assert_eq!(next[..4], [0x86, 0x00, 0x00, 0x10]);
    assert_eq!(next[4 + 128], 0xC6);
    // A delta of 0xFFFFFF or more uses it in a type 2 header.
    let far = write(&mut w, header(6, 9, 1, 0x0200_0010), &payload);
    assert_eq!(far[..8], [0x86, 0xFF, 0xFF, 0xFF, 0x01, 0x00, 0x00, 0x00]);
    assert_eq!(far[8 + 128..8 + 128 + 5], [0xC6, 0x01, 0x00, 0x00, 0x00]);
    let mut r = ChunkReader::new(1024);
    r.push(&out);
    r.push(&next);
    r.push(&far);
    let stamps: Vec<_> = (0..3).map(|_| r.next_message().unwrap().unwrap().header.timestamp).collect();
    assert_eq!(stamps, [0x0100_0000, 0x0100_0010, 0x0200_0010]);
}

#[test]
fn chunk_stream_ids_use_one_two_or_three_bytes() {
    let mut w = ChunkWriter::new();
    assert_eq!(write(&mut w, header(63, 1, 0, 0), &[0; 4])[0], 63);
    assert_eq!(write(&mut w, header(64, 1, 0, 0), &[0; 4])[..2], [0, 0]);
    assert_eq!(write(&mut w, header(319, 1, 0, 0), &[0; 4])[..2], [0, 255]);
    assert_eq!(write(&mut w, header(320, 1, 0, 0), &[0; 4])[..3], [1, 0, 1]);
    assert_eq!(write(&mut w, header(65_599, 1, 0, 0), &[0; 4])[..3], [1, 255, 255]);
    for bad in [0, 1, 65_600] {
        let e = w.write_message(&mut Vec::new(), &header(bad, 1, 0, 0), &[&[0]]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
    }
    let big = vec![0u8; MAX_MESSAGE + 1];
    let mut out = Vec::new();
    assert_eq!(w.write_message(&mut out, &header(5, 9, 1, 0), &[&big]).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    assert!(out.is_empty());
}

/// Accepts at most 7 bytes per write, so every vectored write is partial.
struct Trickle(Vec<u8>);

impl Write for Trickle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len().min(7);
        self.0.extend_from_slice(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn partial_writes_and_many_slices_give_the_same_bytes() {
    // 10 000 bytes at 128 bytes a chunk is 79 chunks, 158 slices: more than one batch.
    let payload: Vec<u8> = (0..10_000u32).map(|i| (i * 31) as u8).collect();
    let (a, b) = payload.split_at(3333);
    let mut whole = Vec::new();
    ChunkWriter::new().write_message(&mut whole, &header(6, 9, 1, 5), &[&payload]).unwrap();
    let mut trickle = Trickle(Vec::new());
    ChunkWriter::new().write_message(&mut trickle, &header(6, 9, 1, 5), &[a, &[], b]).unwrap();
    assert_eq!(trickle.0, whole);
    let mut r = ChunkReader::new(MAX_MESSAGE);
    r.push(&whole);
    assert_eq!(r.next_message().unwrap().unwrap().payload, payload);
}

/// Refuses every write.
struct Refuse;

impl Write for Refuse {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::WouldBlock.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn a_failed_write_starts_its_chunk_stream_again() {
    let mut w = ChunkWriter::new();
    write(&mut w, header(6, 9, 1, 0), &[1; 10]);
    write(&mut w, header(6, 9, 1, 20), &[1; 10]);
    assert!(w.write_message(&mut Refuse, &header(6, 9, 1, 40), &[&[1; 10]]).is_err());
    assert_eq!(write(&mut w, header(6, 9, 1, 40), &[1; 10])[0] >> 6, 0);
    // A new chunk stream whose first write failed starts with type 0 too.
    assert!(w.write_message(&mut Refuse, &header(7, 9, 1, 40), &[&[1; 10]]).is_err());
    assert_eq!(write(&mut w, header(7, 9, 1, 40), &[1; 10])[0] >> 6, 0);
}

#[test]
fn an_empty_message_is_one_header() {
    let mut w = ChunkWriter::new();
    let out = write(&mut w, header(3, 20, 0, 0), &[]);
    assert_eq!(out.len(), 12);
    let mut r = ChunkReader::new(1024);
    r.push(&out);
    assert_eq!(r.next_message().unwrap().unwrap().payload, Vec::<u8>::new());
}

#[test]
fn the_reader_follows_type_3_after_type_0() {
    // §5.3.1.2.4: a type 3 chunk after a type 0 chunk takes the type 0 timestamp as its delta.
    let mut bytes = vec![0x03, 0x00, 0x03, 0xE8, 0x00, 0x00, 0x02, 0x08, 0x01, 0x00, 0x00, 0x00, 0xAA, 0xBB];
    bytes.extend_from_slice(&[0xC3, 0xCC, 0xDD]);
    let mut r = ChunkReader::new(1024);
    r.push(&bytes);
    let a = r.next_message().unwrap().unwrap();
    let b = r.next_message().unwrap().unwrap();
    assert_eq!((a.header.timestamp, a.payload), (1000, vec![0xAA, 0xBB]));
    assert_eq!((b.header.timestamp, b.header.stream_id, b.payload), (2000, 1, vec![0xCC, 0xDD]));
    assert!(r.next_message().unwrap().is_none());
}

#[test]
fn set_chunk_size_and_abort_change_the_reading() {
    let mut w = ChunkWriter::new();
    let mut bytes = write(&mut w, header(2, msg_type::SET_CHUNK_SIZE, 0, 0), &300u32.to_be_bytes());
    w.set_chunk_size(300).unwrap();
    let payload = vec![9u8; 500];
    bytes.extend(write(&mut w, header(6, 9, 1, 0), &payload));
    let mut r = ChunkReader::new(MAX_MESSAGE);
    r.push(&bytes);
    assert_eq!(r.next_message().unwrap().unwrap().header.type_id, msg_type::SET_CHUNK_SIZE);
    assert_eq!(r.chunk_size(), 300);
    assert_eq!(r.next_message().unwrap().unwrap().payload, payload);

    // The first chunk of a 500-byte message, then an Abort for its chunk stream, then a new one.
    let mut w = ChunkWriter::new();
    let first = write(&mut w, header(6, 9, 1, 0), &payload);
    let mut bytes = first[..12 + 128].to_vec();
    bytes.extend(write(&mut w, header(2, msg_type::ABORT, 0, 0), &6u32.to_be_bytes()));
    let mut fresh = ChunkWriter::new();
    bytes.extend(write(&mut fresh, header(6, 9, 1, 40), &[1, 2, 3]));
    let mut r = ChunkReader::new(MAX_MESSAGE);
    r.push(&bytes);
    assert_eq!(r.next_message().unwrap().unwrap().header.type_id, msg_type::ABORT);
    let m = r.next_message().unwrap().unwrap();
    assert_eq!((m.header.timestamp, m.payload), (40, vec![1, 2, 3]));
}

#[test]
fn the_reader_refuses_what_breaks_the_chunk_stream() {
    // A type 1 chunk on a chunk stream with no type 0 header.
    let mut r = ChunkReader::new(1024);
    r.push(&[0x45, 0, 0, 0, 0, 0, 1, 9, 0]);
    assert!(matches!(r.next_message(), Err(Error::Protocol(_))));
    // A message longer than the reader accepts.
    let mut r = ChunkReader::new(100);
    r.push(&write(&mut ChunkWriter::new(), header(5, 9, 1, 0), &[0; 101]));
    assert!(matches!(r.next_message(), Err(Error::Protocol(_))));
    // A new message header while a message is part way through.
    let mut w = ChunkWriter::new();
    let long = write(&mut w, header(6, 9, 1, 0), &[0; 200]);
    let mut bytes = long[..12 + 128].to_vec();
    bytes.extend_from_slice(&[0x06, 0, 0, 0, 0, 0, 1, 9, 1, 0, 0, 0, 0]);
    let mut r = ChunkReader::new(1024);
    r.push(&bytes);
    assert!(matches!(r.next_message(), Err(Error::Protocol(_))));
    // Two messages in progress whose lengths add up past the limit, each within it.
    let mut w = ChunkWriter::new();
    let mut bytes = write(&mut w, header(6, 9, 1, 0), &[0; 600])[..12 + 128].to_vec();
    bytes.extend_from_slice(&write(&mut w, header(7, 9, 1, 0), &[0; 600])[..12 + 128]);
    let mut r = ChunkReader::new(1000);
    r.push(&bytes);
    assert!(matches!(r.next_message(), Err(Error::Protocol(_))));
    // More chunk streams than the reader keeps.
    let mut w = ChunkWriter::new();
    let mut r = ChunkReader::new(1024);
    for csid in 3..3 + crate::MAX_CHUNK_STREAMS as u32 {
        r.push(&write(&mut w, header(csid, 9, 1, 0), &[1]));
        assert!(r.next_message().unwrap().is_some());
    }
    r.push(&write(&mut w, header(3 + crate::MAX_CHUNK_STREAMS as u32, 9, 1, 0), &[1]));
    assert!(matches!(r.next_message(), Err(Error::Protocol(_))));
    // A chunk size of 0.
    let mut r = ChunkReader::new(1024);
    r.push(&write(&mut ChunkWriter::new(), header(2, msg_type::SET_CHUNK_SIZE, 0, 0), &[0x80, 0, 0, 0]));
    assert!(matches!(r.next_message(), Err(Error::Protocol(_))));
}

#[test]
fn the_reader_waits_for_whole_chunks() {
    let mut w = ChunkWriter::new();
    let bytes = write(&mut w, header(320, 9, 1, 0x0100_0000), &[5u8; 300]);
    let mut r = ChunkReader::new(1024);
    for (i, b) in bytes.iter().enumerate() {
        let got = r.next_message().unwrap();
        assert!(got.is_none(), "a message after {i} bytes");
        r.push(&[*b]);
    }
    let m = r.next_message().unwrap().unwrap();
    assert_eq!((m.header.csid, m.header.timestamp, m.payload.len()), (320, 0x0100_0000, 300));
    assert_eq!(r.buffered(), 0);
    assert_eq!(r.received(), bytes.len() as u64);
}

fn message_strategy() -> impl Strategy<Value = (u32, u8, u32, u32, Vec<u8>)> {
    (
        prop_oneof![Just(3u32), Just(4), Just(6), Just(64), Just(400)],
        prop_oneof![Just(8u8), Just(9), Just(18)],
        0u32..3,
        prop_oneof![0u32..100, 0xFF_FF00u32..0x100_0100, Just(u32::MAX)],
        proptest::collection::vec(any::<u8>(), 0..3000),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Random messages at a random chunk size, read back in random pieces, come back whole and in
    /// order, with their headers. Timestamps go forward by small steps, jump past the 24-bit
    /// limit, and sometimes go back.
    #[test]
    fn messages_read_back_whole(
        messages in proptest::collection::vec(message_strategy(), 1..40),
        steps in proptest::collection::vec(prop_oneof![0u32..50, Just(0x100_0000)], 40),
        chunk in prop_oneof![Just(1u32), 2u32..200, Just(4096)],
        pieces in proptest::collection::vec(1usize..5000, 1..50),
    ) {
        let mut w = ChunkWriter::new();
        let mut bytes = Vec::new();
        w.write_message(&mut bytes, &header(2, msg_type::SET_CHUNK_SIZE, 0, 0), &[&chunk.to_be_bytes()]).unwrap();
        w.set_chunk_size(chunk).unwrap();
        let mut expected = Vec::new();
        let mut clock = [0u32; 3];
        for (i, (csid, type_id, stream_id, base, payload)) in messages.into_iter().enumerate() {
            // Mostly forward on each message stream, from a random start.
            let t = if i % 7 == 6 { base } else { clock[stream_id as usize].wrapping_add(steps[i]) };
            clock[stream_id as usize] = t;
            let h = header(csid, type_id, stream_id, t);
            let (a, b) = payload.split_at(payload.len() / 3);
            w.write_message(&mut bytes, &h, &[a, b]).unwrap();
            expected.push(Message { header: h, payload });
        }
        let mut r = ChunkReader::new(MAX_MESSAGE);
        let mut got = Vec::new();
        let mut at = 0;
        let mut i = 0;
        while at < bytes.len() {
            let n = pieces[i % pieces.len()].min(bytes.len() - at);
            r.push(&bytes[at..at + n]);
            at += n;
            i += 1;
            while let Some(m) = r.next_message().unwrap() {
                got.push(m);
            }
        }
        prop_assert_eq!(got.remove(0).header.type_id, msg_type::SET_CHUNK_SIZE);
        prop_assert_eq!(got, expected);
    }
}
