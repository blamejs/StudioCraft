//! Encoded packets shared by reference and fanned out to bounded per-subscriber rings.
//!
//! - [`PacketPool`] turns an encoder's output into a [`SharedPacket`]: the encoder's `Vec<u8>` is
//!   swapped into a pooled packet without being copied, with its [`Info`] (kind, `pts`, `dts`,
//!   keyframe) and a sequence number. Cloning a packet adds one to a reference count.
//! - [`Fanout`] offers each packet to every subscriber's ring. The producer's thread owns it;
//!   [`Control`] adds and removes subscribers from other threads.
//! - [`Receiver`] is one subscriber's end, for example a network sender's thread. It takes packets
//!   with [`Receiver::try_recv`] or parks in [`Receiver::recv_timeout`] until one arrives.
//! - [`Stats`] are each subscriber's counters, read through [`Receiver::stats`] or a [`Monitor`]
//!   without a lock.
//!
//! # Drops
//!
//! Every subscriber may lose packets; none can make the producer wait. Each ring has a packet cap
//! and a byte cap ([`RingConfig`]), and its storage is allocated when it is made. When a ring is
//! full, the producer drops that packet for that subscriber, marks the subscriber as resyncing,
//! and drops every packet after it, audio and data included, until the next video keyframe (in an
//! audio-only stream, the next audio packet). The ring then holds an unbroken run that a decoder
//! can read, so audio and video stay aligned. On the receiving side, a packet more than the ring's
//! span behind the newest packet published is stale: the receiver skips packets from it to the
//! next keyframe that is not stale. Both kinds of loss are counted per subscriber. A new
//! subscriber starts at the next keyframe.
//!
//! All rings draw on one pool. [`Fanout::share`] and [`Fanout::share_copy`] share a packet and,
//! when the pool is full, empty the ring that holds the most bytes, so a stalled subscriber loses
//! its own packets instead of the pool refusing the others' packets. They empty rings only while
//! every packet published came from that pool.
//!
//! # Threads and cost
//!
//! The crate starts no threads. One producer thread shares and publishes packets; each receiver is
//! used by one thread at a time; any thread may hold a [`Control`] or a [`Monitor`].
//!
//! A [`Fanout`] has one producer: [`Fanout::publish`] takes `&mut self`, and packets are published
//! in `dts` order. When video and audio are encoded on two threads, both threads send their packets
//! to one thread, which interleaves them by `dts` and publishes them. Putting the [`Fanout`] behind
//! a lock instead makes each encoder wait for the other while it publishes.
//!
//! - [`Fanout::publish`] reads the subscriber list without a lock and, for each subscriber, adds
//!   one to the packet's reference counts and pushes it into a lock-free bounded queue. It never
//!   blocks or allocates. It wakes a receiver only when that receiver is parked and
//!   [`RingConfig::wake_every`] packets wait; each wake is a system call on the producer's thread.
//!   The fields each side writes for each packet sit on separate cache lines.
//! - [`PacketPool::share`] and [`PacketPool::share_copy`] take a free packet from a free list of
//!   the right size, at the same cost however many packets are in flight. They allocate nothing
//!   once the pool holds as many packets of each size as are in flight at once. Dropping the last
//!   clone of a packet puts it on the pool's lock-free return queue.
//! - Adding or removing a subscriber copies the subscriber list on the thread that asks. The old
//!   list and a ring whose receiver is gone are freed on such a thread, never on the producer's.
//!
//! # Encoders' packets
//!
//! [`Info`] converts from the built-in H.264 encoder's packets (the `h264` feature) and from the
//! hardware encoders' packets (the `hwenc-native` feature), both on by default. A conversion
//! returns a [`ConvertError`] for a packet with no bytes, a decoding time after its presentation
//! time, or a keyframe flag that does not match an H.264 packet's picture type.
//!
//! No `unsafe` code.

#![warn(missing_docs)]

mod convert;
mod fanout;
mod packet;
mod ring;

pub use convert::ConvertError;
pub use fanout::{Control, Fanout};
pub use packet::{Info, Kind, PacketPool, ShareError, SharedPacket};
pub use ring::{
    BATCH_WAIT, ConfigError, MAX_BUFFER_BYTES, MAX_POOL_BYTES, MAX_RING_BYTES, MAX_RING_SLOTS, MAX_SLOTS, MAX_SUBSCRIBERS, Monitor, Receiver,
    RecvError, RingConfig, Stats, StreamRates, SubscriberId,
};

#[cfg(test)]
mod tests;
