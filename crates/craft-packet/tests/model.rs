//! Random streams of packets and reads, checked against a plain model of the drop and stale rules.

use std::collections::VecDeque;

use craft_packet::{Fanout, Info, Kind, PacketPool, RingConfig};
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Op {
    /// Publish a packet of `len` bytes, `step` time units after the previous one.
    Publish { kind: Kind, key: bool, len: usize, step: i64 },
    /// Read up to `n` packets.
    Read(usize),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => (prop_oneof![Just(Kind::Video), Just(Kind::Audio), Just(Kind::Data)], prop::bool::weighted(0.2), 0usize..64, 0i64..20)
            .prop_map(|(kind, key, len, step)| Op::Publish { kind, key: key && kind == Kind::Video, len, step }),
        1 => (0usize..8).prop_map(Op::Read),
    ]
}

/// The rules as plain code: a ring of `cfg`'s size, the producer's drop rule and the receiver's
/// stale rule.
struct Model {
    cfg: RingConfig,
    q: VecDeque<(u64, Info, usize)>,
    bytes: usize,
    joining: bool,
    resync: bool,
    skipping: bool,
    newest: i64,
    dropped: u64,
    skipped: u64,
}

impl Model {
    fn new(cfg: RingConfig) -> Self {
        Self { cfg, q: VecDeque::new(), bytes: 0, joining: true, resync: false, skipping: false, newest: i64::MIN, dropped: 0, skipped: 0 }
    }

    fn starts_run(&self, i: &Info) -> bool {
        if self.cfg.video { i.kind == Kind::Video && i.keyframe } else { i.kind == Kind::Audio }
    }

    fn offer(&mut self, seq: u64, info: Info, len: usize) {
        self.newest = info.dts;
        if (self.joining || self.resync) && !self.starts_run(&info) {
            if !self.joining {
                self.dropped += 1;
            }
            return;
        }
        let full = self.q.len() == self.cfg.slots || (!self.q.is_empty() && self.bytes + len > self.cfg.bytes);
        if full {
            self.joining = false;
            self.resync = true;
            self.dropped += 1;
            return;
        }
        self.joining = false;
        self.resync = false;
        self.bytes += len;
        self.q.push_back((seq, info, len));
    }

    fn read(&mut self) -> Option<u64> {
        while let Some((seq, info, len)) = self.q.pop_front() {
            self.bytes -= len;
            let stale = self.newest.saturating_sub(info.dts) > self.cfg.span;
            if stale {
                self.skipping = true;
            }
            if self.skipping {
                if !stale && self.starts_run(&info) {
                    self.skipping = false;
                } else {
                    self.skipped += 1;
                    continue;
                }
            }
            return Some(seq);
        }
        None
    }
}

proptest! {
    #[test]
    fn the_ring_follows_the_rules(
        slots in 1usize..12,
        bytes in 1usize..256,
        span in 1i64..200,
        video in any::<bool>(),
        ops in prop::collection::vec(op(), 0..400),
    ) {
        let cfg = RingConfig { slots, bytes, span, video };
        let mut fanout = Fanout::new();
        let mut r = fanout.subscribe(cfg).unwrap();
        let mut model = Model::new(cfg);
        let mut pool = PacketPool::new(4, 64).unwrap();
        let mut t = 0i64;
        let payload = [0u8; 64];
        for op in ops {
            match op {
                Op::Publish { kind, key, len, step } => {
                    t += step;
                    let info = Info { kind, pts: t, dts: t, keyframe: key };
                    let p = pool.share_copy(&payload[..len], info).unwrap();
                    model.offer(p.seq(), info, len);
                    fanout.publish(&p);
                }
                Op::Read(n) => {
                    for _ in 0..n {
                        let got = r.try_recv().ok().map(|p| p.seq());
                        prop_assert_eq!(got, model.read());
                    }
                }
            }
            let st = r.stats();
            prop_assert_eq!(st.queued_packets, model.q.len());
            prop_assert_eq!(st.queued_bytes, model.bytes);
            prop_assert_eq!(st.dropped_packets, model.dropped);
            prop_assert_eq!(st.skipped_packets, model.skipped);
            prop_assert!(st.queued_packets <= slots);
            prop_assert!(st.queued_bytes <= bytes || st.queued_packets == 1);
            prop_assert_eq!(st.accepted_packets, st.delivered_packets + st.skipped_packets + st.queued_packets as u64);
        }
    }
}
