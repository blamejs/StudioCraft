# craft-packet

Encoded packets shared by reference and fanned out to bounded per-subscriber rings, for the Craft
apps. One encoder's output goes to several destinations, such as streaming servers. The bytes are
not copied per destination, and no destination waits on another. Every subscriber may lose packets
when it falls behind, so a writer that must keep every packet does not read through a ring. It
contains no `unsafe` code.

License: MIT OR Apache-2.0.

## What it does

- `PacketPool::share` swaps an encoder's `Vec<u8>` into a free packet without copying the bytes,
  returns the `SharedPacket`, and leaves a free packet's previous buffer, of about the same size,
  emptied, in the encoder's `Vec` for the next packet. A buffer more than 4 times larger than its
  packet (and larger than 256 bytes) is copied from instead, and the encoder keeps it.
  `PacketPool::share_copy` copies a slice into a pooled buffer. Each packet carries an `Info`
  (kind, `pts`, `dts`, keyframe) and a sequence number, 1, 2, 3 and on, so a gap shows where
  packets were dropped.
- Cloning a `SharedPacket` adds one to its reference counts. The thread that drops the last clone
  puts the packet on its pool's lock-free return queue.
- `Fanout::publish` offers a packet to every subscriber. The producer's thread owns the `Fanout`;
  `Control` adds and removes subscribers from other threads.
- `Fanout::share` and `Fanout::share_copy` share a packet from a pool. When the pool is full, they
  empty the ring that holds the most bytes and try again, so a stalled subscriber loses its own
  packets and the others keep getting theirs. They empty rings only while every packet published
  came from that pool; after packets from a second pool are published, a full pool returns
  `ShareError::PoolFull`. When an emptied ring returns no packet to the pool, because other rings
  or the caller still hold clones of its packets, they empty no further ring and return
  `ShareError::PoolFull`.
- `Receiver` is one subscriber's end. `try_recv` returns the next packet without waiting;
  `recv_timeout` parks the thread until a packet arrives, the subscription ends or the time is up.
  Dropping the receiver removes the subscription and releases the packets left in its ring, even
  while a `Monitor` of it is kept.
- `Stats` holds each subscriber's counters: packets and bytes accepted, dropped, skipped, delivered,
  discarded and queued, whether the subscriber is resyncing, and how many times the producer woke
  it. `Receiver::stats`, `Monitor::stats` and `Control::monitors` read them without a lock.
  `Monitor::is_waiting` tells whether the receiver is parked.

## Drops

Each ring has a packet cap and a byte cap (`RingConfig`), and its storage is allocated once, when
the subscriber is added. `RingConfig::for_stream` sizes a ring for a number of seconds of a stream:
room for that many seconds of packets, and one and a half times that many seconds of bytes at the
stated bit rate. Three seconds of a 6000 kb/s video and 160 kb/s audio stream at 60 video and 47
audio packets per second is 321 packets and 3 465 000 bytes.

- The producer never blocks. When a packet does not fit, the producer drops it for that subscriber
  and marks the subscriber as resyncing. It then drops every packet for that subscriber, audio and
  data included, until the next video keyframe, and pushes again from that keyframe on. What the
  ring holds is therefore an unbroken run that a decoder can read, and audio and video stay aligned.
  A stream without video (`RingConfig::video` false) resumes at the next audio packet.
- An empty ring takes a packet larger than its byte cap.
- A packet more than `RingConfig::span` behind the newest packet published is stale. The receiver
  skips packets from a stale one to the next keyframe that is not stale.
- A new subscriber starts at the next keyframe. The packets before it are not counted as dropped.
- When `Fanout::share` or `Fanout::share_copy` empties a ring to free pool space, its packets are
  counted as discarded and the subscriber resyncs at the next keyframe, as after an overflow.

## Checks and errors

`PacketPool::new` returns a `ConfigError` for 0 packets, for more than 65 536 packets
(`MAX_SLOTS`), for a buffer over 16 MiB (`MAX_BUFFER_BYTES`), for buffers over 256 MiB together
(`MAX_POOL_BYTES`), and when the system cannot allocate the pool. `Fanout::subscribe` refuses a
ring of 0 packets or more than 32 768 (`MAX_RING_SLOTS`), a byte cap of 0 or more than 128 MiB
(`MAX_RING_BYTES`), a span of 0 or less, a `wake_every` of 0 or more than the ring's packets, and a
65th subscriber (`MAX_SUBSCRIBERS`). One ring therefore never holds more than half of what a pool
can hold. `RingConfig::for_stream` saturates, so rates or times too large give sizes that
`RingConfig::check` refuses: ten minutes of a 6000 kb/s stream is 64 200 packets.

A pool makes more packets when no free one fits, up to 65 536 in all, and its buffers hold at most
256 MiB together (`PacketPool::held_bytes`). When a new buffer would pass that, the pool first
releases its largest free buffers. Past either limit, `share` and `share_copy` return
`ShareError::PoolFull`, and `share` leaves the caller's buffer as it was. `share_copy` returns
`ShareError::TooLarge` for more than 16 MiB of data, and `share` returns
`ShareError::BufferTooLarge` for a buffer whose capacity is more than 16 MiB.

A packet counts against its ring's byte cap until the receiver has finished taking it out. After a
receiver returns `RecvError::Closed`, it returns `Closed` from then on. A packet the producer is
offering while `Control::unsubscribe` runs may reach the ring after that. The receiver's next call
releases it, counting it as discarded, so it does not keep a pool packet.

## Encoders' packets

`Info` converts with `TryFrom` from two encoders' packet types. Each conversion reads the packet's
timestamps and keyframe flag and leaves its bytes in place, so the caller passes the packet's
`data` to `PacketPool::share` next:

```rust
let info = Info::try_from(&packet)?;
let shared = pool.share(&mut packet.data, info)?;
```

- `craft_video::h264::Packet`, from the built-in H.264 encoder (the `h264` feature): `pts` and
  `dts` are kept as the encoder gives them, so pictures reordered for B-frames keep their decoding
  order.
- `craft_hwenc_native::Packet`, from the NVENC, Media Foundation, VideoToolbox and VA-API encoders
  (the `hwenc-native` feature): these encoders do not reorder pictures, so `dts` is the packet's
  `pts`.

Both features are on by default; `default-features = false` builds the pool and the rings alone.
A conversion returns `ConvertError::Empty` for a packet with no bytes and
`ConvertError::DecodedAfterShown` for a `dts` later than its `pts`. An H.264 packet whose keyframe
flag does not match its picture type (only an IDR picture is a keyframe) returns
`ConvertError::KeyframeMismatch`. The HEVC encoder's packet in `craft-video` carries no
timestamps, so an `Info` for it is made with `Info::video`, from the timestamps the caller passed
to the encoder.

## Threads and cost

The crate starts no threads. One producer thread shares and publishes packets; each receiver is used
by one thread at a time; any thread may hold a `Control` or a `Monitor`.

A `Fanout` has one producer. `Fanout::publish`, `Fanout::share` and `Fanout::share_copy` take
`&mut self`, and packets are published in `dts` order. When video and audio are encoded on two
threads, both threads send their packets to one thread, which interleaves them by `dts` and
publishes them. Putting the `Fanout` behind a lock instead makes each encoder wait for the other
while it publishes. A `PacketPool` is also used by one thread at a time. Encoder threads that share
packets from pools of their own call `PacketPool::share`, which returns `ShareError::PoolFull` when
the pool is full. Only `Fanout::share`, on the publishing thread, empties a ring to make room.

- `Fanout::publish` reads the subscriber list without a lock, through `arc-swap`. For each
  subscriber it adds one to the packet's reference counts and pushes the packet into a bounded
  lock-free queue (`crossbeam-queue`'s `ArrayQueue`). It never blocks or allocates. The fields the
  producer writes, the fields the receiver writes and the word both change sit on separate cache
  lines (`crossbeam-utils`' `CachePadded`).
- The producer wakes a parked receiver once `RingConfig::wake_every` packets wait, when its ring is
  full and when the subscription ends. Each wake is a system call on the producer's thread.
  `RingConfig::for_stream` sets `wake_every` to the packets per second divided by 30, so a receiver
  is woken about 30 times a second. When `wake_every` is above 1, a parked receiver also looks at
  its ring every 33 ms (`BATCH_WAIT`), so packets that arrive just before the stream pauses wait at
  most that long.
- The pool keeps free packets in lists by buffer size, in powers of two, so finding one costs the
  same however many packets a stalled subscriber holds. It allocates nothing once it holds as many
  packets of each size as are in flight at once. Until then, sharing a packet may make one more
  packet or replace one free buffer. A pool reserves its tables for 65 536 packets when it is made
  (about 1.8 MiB), so they never move.
- Adding or removing a subscriber copies the subscriber list once, on the thread that asks. The
  replaced list, and a ring whose receiver is gone, are freed on such a thread, never on the
  producer's.
- The `alloc` test counts every allocation and every free while 8 subscribers, one of them never
  reading, take a minute of a 6000 kb/s stream shared from one pool.
- `cargo bench -p craft-packet` measures publishing to 1, 4, 8 and 16 subscribers drained on the
  bench thread, publishing to receivers parked on their own threads, and sharing and publishing
  with and without a stalled ring of 10 000 packets. It fails when a timed part allocates. Each
  timing covers one second of a 60 fps stream (107 packets). On Linux (AMD EPYC 9R14) in a release
  build, on 2026-11-18, publishing that second took 14.47 µs to 8 subscribers (7 drained and
  1 that never reads), 1.952 ms to 8 parked receivers, and 615.8 µs to 8 parked receivers woken
  every 3 packets. The parked timings depend on the load on the host's other cores.

## Dependencies

`crossbeam-queue`, `crossbeam-utils` and `arc-swap`, all MIT OR Apache-2.0. With the default
features it also depends on `craft-video` and `craft-hwenc-native`, MIT OR Apache-2.0. The tests
and benches also use `proptest`, `criterion` and `stats_alloc`.
