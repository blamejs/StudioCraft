//! Chunking one second of a 6000 kb/s, 60 fps H.264 stream with 160 kb/s AAC: 60 video messages
//! of 12 500 bytes and 47 audio messages of 426 bytes, each sent as a codec header and its data,
//! as `Publisher::send_flv` sends them.
//!
//! - `sink`: into `io::sink()`, at the 4096-byte chunk size the publisher sets and at the default
//!   128 bytes. This is the cost of building headers and slices.
//! - `vec`: into a `Vec` that is cleared between iterations and keeps its capacity, which adds the
//!   copy of every byte.
//! - `tcp`: into a loopback TCP socket that a thread drains, which adds the system calls.

use std::hint::black_box;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};

use craft_rtmp::{ChunkWriter, MessageHeader, csid, msg_type};
use criterion::{Criterion, Throughput, criterion_group, criterion_main};

const VIDEO_BYTES: usize = 12_500;
const AUDIO_BYTES: usize = 426;
const AUDIO_MESSAGES: u32 = 47;
const BYTES_PER_SECOND: u64 = 60 * (5 + VIDEO_BYTES as u64) + AUDIO_MESSAGES as u64 * (2 + AUDIO_BYTES as u64);

fn fail(what: &str) -> ! {
    eprintln!("{what}");
    std::process::exit(1)
}

/// Writes one second starting at `second`, video and audio interleaved by time.
fn second<W: Write>(w: &mut ChunkWriter, out: &mut W, second: u32, video: &[u8], audio: &[u8]) {
    let base = second * 1000;
    let mut a = 0;
    for f in 0..60u32 {
        let t = base + f * 1000 / 60;
        let head = [if f == 0 { 0x17 } else { 0x27 }, 1, 0, 0, 0];
        if w.write_message(out, &MessageHeader { csid: csid::VIDEO, type_id: msg_type::VIDEO, stream_id: 1, timestamp: t }, &[&head, video]).is_err()
        {
            fail("a video message was refused");
        }
        while a < AUDIO_MESSAGES && a * 1024 * 1000 / 48_000 <= f * 1000 / 60 {
            let t = base + a * 1024 * 1000 / 48_000;
            let header = MessageHeader { csid: csid::AUDIO, type_id: msg_type::AUDIO, stream_id: 1, timestamp: t };
            if w.write_message(out, &header, &[&[0xAF, 1], audio]).is_err() {
                fail("an audio message was refused");
            }
            a += 1;
        }
    }
}

fn writer(chunk: u32) -> ChunkWriter {
    let mut w = ChunkWriter::new();
    if w.set_chunk_size(chunk).is_err() {
        fail("the chunk size was refused");
    }
    w
}

fn benches(c: &mut Criterion) {
    let video = vec![0x41u8; VIDEO_BYTES];
    let audio = vec![0x21u8; AUDIO_BYTES];

    let mut g = c.benchmark_group("sink");
    g.throughput(Throughput::Bytes(BYTES_PER_SECOND));
    for chunk in [4096, 128] {
        let mut w = writer(chunk);
        let mut s = 0;
        g.bench_function(format!("chunk 1 s at {chunk} bytes"), |b| {
            b.iter(|| {
                second(&mut w, &mut io::sink(), s, black_box(&video), black_box(&audio));
                s += 1;
            })
        });
    }
    g.finish();

    let mut g = c.benchmark_group("vec");
    g.throughput(Throughput::Bytes(BYTES_PER_SECOND));
    let mut w = writer(4096);
    let mut out = Vec::with_capacity(1 << 20);
    let mut s = 0;
    g.bench_function("chunk 1 s at 4096 bytes into a Vec", |b| {
        b.iter(|| {
            out.clear();
            second(&mut w, &mut out, s, black_box(&video), black_box(&audio));
            s += 1;
            black_box(out.len())
        })
    });
    g.finish();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap_or_else(|_| fail("no loopback port"));
    let addr = listener.local_addr().unwrap_or_else(|_| fail("no loopback address"));
    let drain = std::thread::spawn(move || {
        let Ok((mut s, _)) = listener.accept() else { return };
        let mut buf = vec![0u8; 1 << 16];
        while matches!(s.read(&mut buf), Ok(n) if n > 0) {}
    });
    let mut socket = TcpStream::connect(addr).unwrap_or_else(|_| fail("no loopback connection"));
    socket.set_nodelay(true).ok();
    let mut g = c.benchmark_group("tcp");
    g.throughput(Throughput::Bytes(BYTES_PER_SECOND));
    let mut w = writer(4096);
    let mut s = 0;
    g.bench_function("send 1 s at 4096 bytes over loopback TCP", |b| {
        b.iter(|| {
            second(&mut w, &mut socket, s, black_box(&video), black_box(&audio));
            s += 1;
        })
    });
    g.finish();
    drop(socket);
    drain.join().ok();
}

criterion_group!(chunk, benches);
criterion_main!(chunk);
