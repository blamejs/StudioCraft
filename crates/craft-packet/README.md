# craft-packet

Encoded packets shared by reference and fanned out to bounded per-subscriber rings, for the Craft
apps. One encoder's output goes to several destinations, such as streaming servers or a recording.
The bytes are not copied per destination, and no destination waits on another. It contains no
`unsafe` code.

License: MIT OR Apache-2.0.

## What it does

- `PacketPool::share` swaps an encoder's `Vec<u8>` into a free packet without copying the bytes,
  returns the `SharedPacket`, and leaves the packet's previous buffer, emptied, in the encoder's
  `Vec` for the next packet. `PacketPool::share_copy` copies a slice into a pooled buffer instead. Each packet carries an `Info` (kind, `pts`, `dts`, keyframe) and a sequence
  number, 1, 2, 3 and on, so a gap shows where packets were dropped.
- Cloning a `SharedPacket` adds one to a reference count. The packet goes back to its pool when the
  last clone is dropped; the pool sees this from the reference count.
- `Fanout::publish` offers a packet to every subscriber. The producer's thread owns the `Fanout`;
  `Control` adds and removes subscribers from other threads.
- `Receiver` is one subscriber's end. `try_recv` returns the next packet without waiting;
  `recv_timeout` parks the thread until a packet arrives, the subscription ends or the time is up.
  Dropping the receiver removes the subscription.
- `Stats` holds each subscriber's counters: packets and bytes accepted, dropped, skipped, delivered
  and queued, and whether the subscriber is resyncing. `Receiver::stats`, `Monitor::stats` and
  `Control::monitors` read them without a lock.

## Drops

Each ring has a packet cap and a byte cap (`RingConfig`), and its storage is allocated once, when the
subscriber is added. `RingConfig::for_stream` sizes a ring for a number of seconds of a stream: room
for that many seconds of packets, and one and a half times that many seconds of bytes at the stated
bit rate. Three seconds of a 6000 kb/s video and 160 kb/s audio stream at 60 video and 47 audio
packets per second is 321 packets and 3 465 000 bytes.

- The producer never blocks. When a packet does not fit, the producer drops it for that subscriber
  and marks the subscriber as resyncing. It then drops every packet for that subscriber, audio and
  data included, until the next video keyframe, and pushes again from that keyframe on. What the
  ring holds is therefore an unbroken run that a decoder can read, and audio and video stay aligned.
  A stream without video (`RingConfig::video` false) resumes at the next audio packet.
- An empty ring takes a packet larger than its byte cap.
- A packet more than `RingConfig::span` behind the newest packet published is stale. The receiver
  skips packets from a stale one to the next keyframe that is not stale.
- A new subscriber starts at the next keyframe. The packets before it are not counted as dropped.

## Checks and errors

`PacketPool::new` and `Fanout::subscribe` return a `ConfigError` for 0 packets, for more than
65 536 packets (`MAX_SLOTS`), for a ring with a byte cap or span of 0 or less, for a pool buffer
over 16 MiB (`MAX_BUFFER_BYTES`), for pool buffers over 256 MiB together (`MAX_POOL_BYTES`), and
when the system cannot allocate the pool or the ring. `Fanout::subscribe` also refuses a 65th
subscriber (`MAX_SUBSCRIBERS`). `RingConfig::for_stream` saturates, so rates too large give a
packet count that `RingConfig::check` refuses.

A pool makes more packets when all of its packets are in flight, up to 65 536 in all, and its
buffers hold at most 256 MiB together (`PacketPool::held_bytes`). Past either limit, `share` and
`share_copy` return `ShareError::PoolFull`, and `share` leaves the caller's buffer as it was.
`share_copy` returns `ShareError::TooLarge` for more than 16 MiB of data, and `share` returns
`ShareError::BufferTooLarge` for a buffer whose capacity is more than 16 MiB.

A packet counts against its ring's byte cap until the receiver has finished taking it out. After a
receiver returns `RecvError::Closed`, it returns `Closed` from then on. A packet the producer is
offering while `Control::unsubscribe` runs may reach the ring after that and is not delivered.

## Threads and cost

The crate starts no threads. One producer thread shares and publishes packets; each receiver is used
by one thread at a time; any thread may hold a `Control` or a `Monitor`.

- `Fanout::publish` reads the subscriber list without a lock, through `arc-swap`. For each subscriber
  it adds one to the packet's reference count and pushes the packet into a bounded lock-free queue
  (`crossbeam-queue`'s `ArrayQueue`). It never blocks. It wakes a receiver only when that receiver
  is parked.
- Once the pool holds as many packets as are in flight at once, publishing allocates nothing: the
  `alloc` test counts every allocation while 8 subscribers, one of them never reading, take a minute
  of a 6000 kb/s stream.
- Adding or removing a subscriber copies the subscriber list once, on the thread that asks.
- `cargo bench -p craft-packet` publishes one second of a 60 fps stream (107 packets) to 1, 4, 8 and
  16 subscribers, one of which never reads, and fails when the timed part allocates. The results are
  in StudioCraft's `docs/performance.md`.

## Dependencies

`crossbeam-queue` and `arc-swap`, both MIT OR Apache-2.0. The tests and benches also use
`proptest`, `criterion` and `stats_alloc`.
