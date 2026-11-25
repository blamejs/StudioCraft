//! The jitter buffer of one RTP stream, and the counts its receiver reports carry (RFC 3550
//! sections 6.4.1, A.1, A.3 and A.8).

use std::time::Duration;

use crate::rtcp::{MAX_CUMULATIVE_LOST, MIN_CUMULATIVE_LOST, ReportBlock, ntp_middle};
use crate::{Error, Packet};

/// Sequence numbers the buffer can hold at once, from the oldest it waits for to the newest. A
/// packet further ahead makes the buffer give up on the oldest ones.
pub const SLOTS: usize = 4096;
/// The largest jump ahead in sequence numbers that counts as packets lost in between rather than
/// a sender that started again (RFC 3550 A.1).
pub const MAX_DROPOUT: u16 = 3000;
/// The furthest behind the newest sequence number a packet counts as reordered rather than a
/// sender that started again (RFC 3550 A.1).
pub const MAX_MISORDER: u16 = 100;

/// The delay before enough packets have arrived to measure the jitter.
pub const START_DELAY: Duration = Duration::from_millis(60);
/// The shortest delay.
pub const MIN_DELAY: Duration = Duration::from_millis(40);
/// The longest delay.
pub const MAX_DELAY: Duration = Duration::from_millis(150);
/// The most time between the oldest and the newest packet held.
pub const MAX_SPAN: Duration = Duration::from_secs(2);
/// The most payload bytes held.
pub const MAX_BYTES: usize = 8 * 1024 * 1024;
/// The delay is this many times the jitter estimate, inside [`MIN_DELAY`] to [`MAX_DELAY`].
const JITTER_FACTOR: u128 = 4;
/// Packets measured before the delay follows the jitter estimate.
const JITTER_SAMPLES: u32 = 16;
/// Released payload buffers kept for reuse. Together they hold no more than
/// [`Config::max_bytes`].
const SPARE_BUFFERS: usize = 64;

/// The settings of a [`JitterBuffer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The stream's RTP clock rate in Hz, such as 90 000 for video and 48 000 for Opus.
    pub clock_rate: u32,
    /// The delay before the jitter is measured.
    pub start_delay: Duration,
    /// The shortest delay.
    pub min_delay: Duration,
    /// The longest delay.
    pub max_delay: Duration,
    /// The most time between the oldest and the newest packet held.
    pub max_span: Duration,
    /// The most payload bytes held, counted as the memory allocated for the payloads of the
    /// packets held.
    pub max_bytes: usize,
}

impl Config {
    /// The default settings for a stream whose RTP clock runs at `clock_rate` Hz: a delay of
    /// 60 ms at first, then four times the jitter estimate between 40 and 150 ms, and at most
    /// 2 s or 8 MiB of payload held.
    pub fn new(clock_rate: u32) -> Config {
        Config { clock_rate, start_delay: START_DELAY, min_delay: MIN_DELAY, max_delay: MAX_DELAY, max_span: MAX_SPAN, max_bytes: MAX_BYTES }
    }
}

/// What [`JitterBuffer::push`] did with a packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Push {
    /// The packet is held until its time.
    Stored,
    /// A packet with this sequence number is already held, and this one was dropped.
    Duplicate,
    /// The packet came after its sequence number was released or given up on, and was dropped.
    Late,
    /// The sequence number jumped by more than [`MAX_DROPOUT`] ahead or [`MAX_MISORDER`] behind,
    /// and the packet was dropped. When the next packet follows it in sequence, the sender is
    /// taken to have started again.
    Stray,
    /// The packet followed a stray one in sequence, so the buffer dropped what it held and
    /// started again from this packet, which it holds. The next event is a gap.
    Restarted,
    /// The payload is larger than [`Config::max_bytes`] and was not copied. The packet keeps its
    /// place in sequence, counts as received in the report block and is not listed for a NACK,
    /// and [`JitterBuffer::pop`] releases it as a gap of one. The packets already held stay. A
    /// packet that makes the buffer start again gives [`Push::Restarted`] instead.
    Oversized,
}

/// A packet the buffer releases in sequence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Released<'a> {
    /// Its sequence number.
    pub sequence: u16,
    /// Its RTP timestamp.
    pub timestamp: u32,
    /// Its marker bit.
    pub marker: bool,
    /// Its payload type.
    pub payload_type: u8,
    /// When it arrived, on the caller's clock.
    pub arrival: Duration,
    /// Its payload.
    pub payload: &'a [u8],
}

/// What [`JitterBuffer::pop`] gives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event<'a> {
    /// The next packet in sequence.
    Packet(Released<'a>),
    /// The stream continues after a break: `missing` sequence numbers did not arrive in time or
    /// were dropped, or the sender started again (then `missing` counts the packets dropped and
    /// may be 0). A depacketizer drops the frame it is building.
    Gap {
        /// Sequence numbers skipped.
        missing: u64,
    },
}

/// The counts of a [`JitterBuffer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    /// Packets passed to [`JitterBuffer::push`].
    pub received: u64,
    /// Packets released.
    pub released: u64,
    /// Sequence numbers given up on because their packet did not arrive in time.
    pub missing: u64,
    /// Packets that came after their sequence number was released or given up on.
    pub late: u64,
    /// Packets whose sequence number was already held.
    pub duplicates: u64,
    /// Packets whose sequence number jumped too far.
    pub stray: u64,
    /// Packets held and then dropped because the buffer was over [`Config::max_bytes`] or
    /// [`Config::max_span`], more than [`SLOTS`] ahead, or the sender started again, and packets
    /// whose payload alone was larger than [`Config::max_bytes`], counted when they are released
    /// as a gap.
    pub dropped: u64,
    /// Times the sender started again.
    pub restarts: u64,
    /// The inter-arrival jitter estimate in RTP timestamp units (RFC 3550 section 6.4.1).
    pub jitter: u32,
    /// The delay each packet is held for now.
    pub delay: Duration,
}

/// The last sender report received from the stream's sender, for a receiver report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LastSenderReport {
    /// Its 64-bit NTP timestamp.
    pub ntp: u64,
    /// When it arrived, on the caller's clock.
    pub received_at: Duration,
}

#[derive(Debug)]
struct Held {
    arrival: Duration,
    sequence: u16,
    timestamp: u32,
    marker: bool,
    payload_type: u8,
    data: Vec<u8>,
    /// The payload was over [`Config::max_bytes`] and was not copied: the packet keeps its place
    /// in sequence and is released as a gap.
    oversized: bool,
}

#[derive(Debug)]
enum Slot {
    Empty { asked: bool },
    Full(Held),
}

/// Puts the packets of one RTP stream (one synchronization source) back in sequence order.
///
/// Every packet is held for the current delay after it arrives, so later packets can fill the
/// gaps before it. A missing packet is waited for until the delay has passed since the first
/// packet after it arrived; [`nacks`](JitterBuffer::nacks) lists it once so the caller can ask
/// the sender for it. The delay starts at [`Config::start_delay`] and, once 16 packets have been
/// measured, is four times the jitter estimate, kept inside [`Config::min_delay`] to
/// [`Config::max_delay`].
///
/// The caller passes the time to every call, as a [`Duration`] since any fixed point of its
/// choosing, so a test drives the buffer with a clock of its own. Times that go back are treated
/// as the latest time passed in.
#[derive(Debug)]
pub struct JitterBuffer {
    config: Config,
    slots: Vec<Slot>,
    spare: Vec<Vec<u8>>,
    /// The capacity of the buffers in `spare`, at most [`Config::max_bytes`].
    spare_bytes: usize,
    out: Vec<u8>,
    started: bool,
    head: i64,
    highest: i64,
    held: usize,
    bytes: usize,
    /// The capacity of the buffers of the packets held, at least `bytes`. It is kept within
    /// [`Config::max_bytes`].
    held_bytes: usize,
    gap: Option<u64>,
    bad_seq: Option<u16>,
    now: Duration,
    last_transit: Option<i64>,
    jitter_q4: u32,
    samples: u32,
    delay: Duration,
    base: i64,
    received_since_base: u64,
    expected_prior: u64,
    received_prior: u64,
    stats: Stats,
}

impl JitterBuffer {
    /// A buffer with `config`. A clock rate of 0, a shortest delay longer than the longest, and a
    /// span or byte limit of 0 are refused. The start delay is kept inside the shortest and
    /// longest delays.
    pub fn new(config: Config) -> Result<JitterBuffer, Error> {
        if config.clock_rate == 0 {
            return Err(Error::BadInput("the RTP clock rate is 0"));
        }
        if config.min_delay > config.max_delay {
            return Err(Error::BadInput("the shortest delay is longer than the longest"));
        }
        if config.max_span.is_zero() || config.max_bytes == 0 {
            return Err(Error::BadInput("the jitter buffer's span and byte limits are above 0"));
        }
        let delay = config.start_delay.clamp(config.min_delay, config.max_delay);
        Ok(JitterBuffer {
            config,
            slots: (0..SLOTS).map(|_| Slot::Empty { asked: false }).collect(),
            spare: Vec::new(),
            spare_bytes: 0,
            out: Vec::new(),
            started: false,
            head: 0,
            highest: 0,
            held: 0,
            bytes: 0,
            held_bytes: 0,
            gap: None,
            bad_seq: None,
            now: Duration::ZERO,
            last_transit: None,
            jitter_q4: 0,
            samples: 0,
            delay,
            base: 0,
            received_since_base: 0,
            expected_prior: 0,
            received_prior: 0,
            stats: Stats { delay, ..Stats::default() },
        })
    }

    /// The buffer's settings.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Takes a packet that arrived at `now`.
    pub fn push(&mut self, packet: &Packet<'_>, now: Duration) -> Push {
        let now = self.tick(now);
        self.stats.received += 1;
        if !self.started {
            self.start_at(i64::from(packet.sequence));
            let stored = self.store(self.head, packet, now);
            self.enforce_limits(now);
            return stored;
        }
        let u = packet.sequence.wrapping_sub(self.highest as u16);
        let ext = if u < MAX_DROPOUT {
            self.highest + i64::from(u)
        } else if u32::from(u) <= 0x1_0000 - u32::from(MAX_MISORDER) {
            if self.bad_seq == Some(packet.sequence) {
                self.restart(i64::from(packet.sequence));
                self.store(self.head, packet, now);
                self.enforce_limits(now);
                return Push::Restarted;
            }
            self.bad_seq = Some(packet.sequence.wrapping_add(1));
            self.stats.stray += 1;
            return Push::Stray;
        } else {
            self.highest - (0x1_0000 - i64::from(u))
        };
        self.bad_seq = None;
        if ext < self.head {
            self.stats.late += 1;
            if ext >= self.base {
                self.received_since_base += 1;
            }
            return Push::Late;
        }
        while ext - self.head >= SLOTS as i64 {
            self.drop_head();
        }
        if matches!(self.slot(ext), Some(Slot::Full(_))) {
            self.stats.duplicates += 1;
            // RFC 3550 section 6.4.1: the received count of a report includes duplicates.
            self.received_since_base += 1;
            return Push::Duplicate;
        }
        let stored = self.store(ext, packet, now);
        self.highest = self.highest.max(ext);
        self.enforce_limits(now);
        stored
    }

    /// The next event at `now`: a gap, or the next packet in sequence once its delay has passed,
    /// or `None` while the next packet is not due. The payload is borrowed until the next call.
    pub fn pop(&mut self, now: Duration) -> Option<Event<'_>> {
        let now = self.tick(now);
        if let Some(missing) = self.gap.take() {
            return Some(Event::Gap { missing });
        }
        if self.held == 0 {
            return None;
        }
        let delay = self.delay;
        match self.slot(self.head) {
            Some(Slot::Full(h)) if now >= h.arrival.saturating_add(delay) => {}
            Some(Slot::Full(_)) => return None,
            _ => {
                let (next, arrival) = self.next_held_earliest()?;
                if now < arrival.saturating_add(delay) {
                    return None;
                }
                let missing = (next - self.head) as u64;
                for ext in self.head..next {
                    self.set_slot(ext, Slot::Empty { asked: false });
                }
                self.head = next;
                self.stats.missing += missing;
                return Some(Event::Gap { missing });
            }
        }
        let Some(Slot::Full(h)) = self.take_slot(self.head) else { return None };
        self.head += 1;
        self.held -= 1;
        if h.oversized {
            self.stats.dropped += 1;
            return Some(Event::Gap { missing: 1 });
        }
        self.bytes -= h.data.len();
        self.held_bytes -= h.data.capacity();
        self.stats.released += 1;
        let old = std::mem::replace(&mut self.out, h.data);
        self.recycle(old);
        Some(Event::Packet(Released {
            sequence: h.sequence,
            timestamp: h.timestamp,
            marker: h.marker,
            payload_type: h.payload_type,
            arrival: h.arrival,
            payload: &self.out,
        }))
    }

    /// The earliest time at which [`pop`](JitterBuffer::pop) gives an event, or `None` while the
    /// buffer is empty. [`Duration::ZERO`] when one is due now.
    pub fn next_wake(&self) -> Option<Duration> {
        if self.gap.is_some() {
            return Some(Duration::ZERO);
        }
        if self.held == 0 {
            return None;
        }
        let arrival = match self.slot(self.head) {
            Some(Slot::Full(h)) => h.arrival,
            _ => self.next_held_earliest()?.1,
        };
        let at = arrival.saturating_add(self.delay);
        Some(if at <= self.now { Duration::ZERO } else { at })
    }

    /// Appends to `out` the sequence numbers, oldest first, that are missing between the oldest
    /// one waited for and the newest held, and have not been listed before, up to `max` of them.
    /// Returns how many it appended. Each number is listed once, for one NACK.
    pub fn nacks(&mut self, out: &mut Vec<u16>, max: usize) -> usize {
        let mut n = 0;
        let mut ext = self.head;
        while ext < self.highest && n < max {
            if let Some(Slot::Empty { asked: asked @ false }) = self.slot_mut(ext) {
                *asked = true;
                out.push(ext as u16);
                n += 1;
            }
            ext += 1;
        }
        n
    }

    /// The report block for a receiver report on this stream, sent to `ssrc`, with the last
    /// sender report received from it, or `None` before the first packet. The fraction lost
    /// covers the time since the last call.
    pub fn report_block(&mut self, ssrc: u32, last_sr: Option<LastSenderReport>, now: Duration) -> Option<ReportBlock> {
        if !self.started {
            return None;
        }
        let now = self.tick(now);
        let expected = (self.highest - self.base + 1).max(0) as u64;
        let lost = expected as i64 - self.received_since_base as i64;
        let expected_interval = expected.saturating_sub(self.expected_prior);
        let received_interval = self.received_since_base.saturating_sub(self.received_prior);
        self.expected_prior = expected;
        self.received_prior = self.received_since_base;
        let lost_interval = expected_interval as i64 - received_interval as i64;
        let fraction_lost =
            if expected_interval == 0 || lost_interval <= 0 { 0 } else { ((lost_interval as u64) * 256 / expected_interval).min(255) as u8 };
        let (last_sr, delay_since_last_sr) = match last_sr {
            Some(sr) => {
                let since = now.saturating_sub(sr.received_at).as_nanos() * 65_536 / 1_000_000_000;
                (ntp_middle(sr.ntp), u32::try_from(since).unwrap_or(u32::MAX))
            }
            None => (0, 0),
        };
        Some(ReportBlock {
            ssrc,
            fraction_lost,
            cumulative_lost: lost.clamp(i64::from(MIN_CUMULATIVE_LOST), i64::from(MAX_CUMULATIVE_LOST)) as i32,
            highest_sequence: self.highest as u32,
            jitter: self.jitter_q4 >> 4,
            last_sr,
            delay_since_last_sr,
        })
    }

    /// The buffer's counts.
    pub fn stats(&self) -> Stats {
        Stats { jitter: self.jitter_q4 >> 4, delay: self.delay, ..self.stats }
    }

    /// The delay each packet is held for now.
    pub fn delay(&self) -> Duration {
        self.delay
    }

    /// Packets held, [`Push::Oversized`] ones included without their payload.
    pub fn held(&self) -> usize {
        self.held
    }

    /// Payload bytes held.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    fn tick(&mut self, now: Duration) -> Duration {
        self.now = self.now.max(now);
        self.now
    }

    fn index(ext: i64) -> usize {
        ext.rem_euclid(SLOTS as i64) as usize
    }

    fn slot(&self, ext: i64) -> Option<&Slot> {
        self.slots.get(Self::index(ext))
    }

    fn slot_mut(&mut self, ext: i64) -> Option<&mut Slot> {
        self.slots.get_mut(Self::index(ext))
    }

    fn set_slot(&mut self, ext: i64, slot: Slot) {
        if let Some(s) = self.slot_mut(ext) {
            *s = slot;
        }
    }

    fn take_slot(&mut self, ext: i64) -> Option<Slot> {
        self.slot_mut(ext).map(|s| std::mem::replace(s, Slot::Empty { asked: false }))
    }

    /// The first held packet after the head, and its arrival.
    fn next_held(&self) -> Option<(i64, Duration)> {
        (self.head..=self.highest).find_map(|ext| match self.slot(ext) {
            Some(Slot::Full(h)) => Some((ext, h.arrival)),
            _ => None,
        })
    }

    /// The first held packet after the head, and the earliest arrival of the packets held after
    /// the head: a missing packet at the head is waited for from the first packet after it to
    /// arrive, which may be later in sequence than the first one held.
    fn next_held_earliest(&self) -> Option<(i64, Duration)> {
        let mut next = None;
        for ext in self.head..=self.highest {
            if let Some(Slot::Full(h)) = self.slot(ext) {
                next = match next {
                    None => Some((ext, h.arrival)),
                    Some((first, arrival)) => Some((first, h.arrival.min(arrival))),
                };
            }
        }
        next
    }

    fn start_at(&mut self, ext: i64) {
        self.started = true;
        self.head = ext;
        self.highest = ext;
        self.base = ext;
        self.received_since_base = 0;
        self.expected_prior = 0;
        self.received_prior = 0;
        self.bad_seq = None;
    }

    fn restart(&mut self, ext: i64) {
        let mut dropped = 0;
        for ext in self.head..=self.highest {
            if let Some(Slot::Full(h)) = self.take_slot(ext) {
                dropped += 1;
                self.recycle(h.data);
            }
        }
        self.held = 0;
        self.bytes = 0;
        self.held_bytes = 0;
        self.stats.dropped += dropped;
        self.stats.restarts += 1;
        self.gap = Some(self.gap.unwrap_or(0) + dropped);
        self.last_transit = None;
        self.start_at(ext);
    }

    /// Holds the packet at `ext`. A payload over [`Config::max_bytes`] is not copied: the packet
    /// keeps its place in sequence without it, and is released as a gap.
    fn store(&mut self, ext: i64, packet: &Packet<'_>, now: Duration) -> Push {
        let len = packet.payload.len();
        let oversized = len > self.config.max_bytes;
        let data = if oversized {
            Vec::new()
        } else {
            let mut data = self.take_spare(len);
            data.clear();
            data.reserve_exact(len);
            data.extend_from_slice(packet.payload);
            data
        };
        self.bytes += data.len();
        self.held_bytes += data.capacity();
        self.held += 1;
        self.received_since_base += 1;
        let held = Held {
            arrival: now,
            sequence: packet.sequence,
            timestamp: packet.timestamp,
            marker: packet.marker,
            payload_type: packet.payload_type,
            data,
            oversized,
        };
        self.set_slot(ext, Slot::Full(held));
        self.measure(packet.timestamp, now);
        if oversized { Push::Oversized } else { Push::Stored }
    }

    /// The inter-arrival jitter of RFC 3550 section 6.4.1, kept as 16 times its value as in
    /// section A.8. A difference over one second of the RTP clock counts as one second.
    fn measure(&mut self, timestamp: u32, now: Duration) {
        let rate = u128::from(self.config.clock_rate);
        let arrival = (now.as_nanos() * rate / 1_000_000_000) as u32;
        let transit = i64::from(arrival.wrapping_sub(timestamp) as i32);
        if let Some(last) = self.last_transit {
            // Transit times are 32-bit RTP clock values, so their difference wraps.
            let d = (transit as i32).wrapping_sub(last as i32).unsigned_abs().min(self.config.clock_rate);
            self.jitter_q4 = self.jitter_q4.saturating_add(d).saturating_sub(self.jitter_q4.saturating_add(8) >> 4);
            self.samples = self.samples.saturating_add(1);
        }
        self.last_transit = Some(transit);
        if self.samples >= JITTER_SAMPLES {
            let nanos = u128::from(self.jitter_q4 >> 4) * 1_000_000_000 / rate * JITTER_FACTOR;
            let delay = Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX));
            self.delay = delay.clamp(self.config.min_delay, self.config.max_delay);
        }
    }

    fn enforce_limits(&mut self, now: Duration) {
        while self.held > 0 {
            let oldest = match self.slot(self.head) {
                Some(Slot::Full(h)) => Some(h.arrival),
                _ => self.next_held().map(|(_, a)| a),
            };
            let span = oldest.map_or(Duration::ZERO, |a| now.saturating_sub(a));
            if self.held_bytes <= self.config.max_bytes && span <= self.config.max_span {
                break;
            }
            self.drop_head();
        }
    }

    /// Gives up on the oldest sequence number waited for, dropping its packet if it is held.
    fn drop_head(&mut self) {
        match self.take_slot(self.head) {
            Some(Slot::Full(h)) => {
                self.held -= 1;
                self.bytes -= h.data.len();
                self.held_bytes -= h.data.capacity();
                self.stats.dropped += 1;
                self.recycle(h.data);
            }
            _ => self.stats.missing += 1,
        }
        self.gap = Some(self.gap.unwrap_or(0) + 1);
        self.head += 1;
        self.highest = self.highest.max(self.head - 1);
    }

    /// A spare buffer for a payload of `len` bytes, or a new empty one. A spare buffer is reused
    /// when it is no larger than `len`, or no larger than twice `len` and the packets held would
    /// stay within [`Config::max_bytes`] with it.
    fn take_spare(&mut self, len: usize) -> Vec<u8> {
        let room = self.config.max_bytes.saturating_sub(self.held_bytes);
        let fits = |capacity: usize| capacity <= len || (capacity <= len.saturating_mul(2) && capacity <= room);
        match self.spare.iter().rposition(|b| fits(b.capacity())) {
            Some(i) => {
                let data = self.spare.swap_remove(i);
                self.spare_bytes -= data.capacity();
                data
            }
            None => Vec::new(),
        }
    }

    fn recycle(&mut self, mut data: Vec<u8>) {
        let capacity = data.capacity();
        if capacity > 0 && self.spare.len() < SPARE_BUFFERS && capacity <= self.config.max_bytes.saturating_sub(self.spare_bytes) {
            data.clear();
            self.spare_bytes += capacity;
            self.spare.push(data);
        }
    }

    /// The bytes allocated for the payload buffers kept for reuse.
    #[cfg(test)]
    pub(crate) fn spare_capacity(&self) -> usize {
        debug_assert_eq!(self.spare_bytes, self.spare.iter().map(Vec::capacity).sum::<usize>());
        self.spare_bytes
    }

    /// The bytes allocated for the payload buffers of the packets held.
    #[cfg(test)]
    pub(crate) fn held_capacity(&self) -> usize {
        self.slots
            .iter()
            .map(|s| match s {
                Slot::Full(h) => h.data.capacity(),
                Slot::Empty { .. } => 0,
            })
            .sum()
    }
}
