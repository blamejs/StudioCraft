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
//! Each ring has a packet cap and a byte cap ([`RingConfig`]), and its storage is allocated when
//! it is made. The producer never blocks. When a ring is full, the producer drops that packet for
//! that subscriber, marks the subscriber as resyncing, and drops every packet after it, audio and
//! data included, until the next video keyframe (in an audio-only stream, the next audio packet).
//! The ring then holds an unbroken run that a decoder can read, so audio and video stay aligned.
//! On the receiving side, a packet more than the ring's span behind the newest packet published
//! is stale: the receiver skips packets from it to the next keyframe that is not stale. Both kinds
//! of loss are counted per subscriber. A new subscriber starts at the next keyframe.
//!
//! # Threads and cost
//!
//! The crate starts no threads. One producer thread shares and publishes packets; each receiver is
//! used by one thread at a time; any thread may hold a [`Control`] or a [`Monitor`].
//!
//! - [`Fanout::publish`] reads the subscriber list without a lock and, for each subscriber, adds
//!   one to the packet's reference count and pushes it into a lock-free bounded queue. It never
//!   blocks and, after warm-up, allocates nothing. It wakes a receiver only when that receiver is
//!   parked.
//! - [`PacketPool::share`] and [`PacketPool::share_copy`] allocate nothing once the pool holds as
//!   many packets as are in flight at once (and, for `share_copy`, once the buffers are large
//!   enough). Dropping a packet costs one atomic decrement.
//! - Adding or removing a subscriber copies the subscriber list, off the producer's thread.
//!
//! No `unsafe` code.

#![warn(missing_docs)]

mod fanout;
mod packet;
mod ring;

pub use fanout::{Control, Fanout};
pub use packet::{Info, Kind, PacketPool, ShareError, SharedPacket};
pub use ring::{
    ConfigError, MAX_BUFFER_BYTES, MAX_POOL_BYTES, MAX_SLOTS, MAX_SUBSCRIBERS, Monitor, Receiver, RecvError, RingConfig, Stats, StreamRates,
    SubscriberId,
};

#[cfg(test)]
mod tests;
