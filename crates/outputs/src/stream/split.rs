//! Hands the frames and the sound a stream receives to each of its video encoders: the stream's
//! own, and those destinations run for themselves ([`studiocraft_plugin::OwnEncoder`]).
//!
//! - A stream whose destinations all send what the stream's encoder makes has no split: the
//!   studio feeds that encoder's queues directly, so the split costs it nothing.
//! - Otherwise one thread takes each frame from the stream's video queue and offers it to every
//!   encoder's queue; a frame is shared, not copied. It takes each block of sound and copies it
//!   for every encoder but the last, which gets the block itself. Each encoder's copies come from
//!   a pool as large as its queue, so after the pools have warmed up a block is copied without
//!   allocating, however far an encoder falls behind.
//! - The thread takes at most a queue's worth of frames, then of sound, before it turns to the
//!   other, so a burst of one never starves the other.
//! - An encoder added while the stream runs ([`Split::add`]) gets frames from the next one on. The
//!   thread picks it up through a bounded queue, so adding one never waits for the thread.
//! - Every queue drops its oldest item when full, so an encoder that falls behind costs itself
//!   frames and never stalls the split, the other encoders or the compositor.
//! - The split holds an encoder's queues weakly. Once the encoder is dropped, the split stops
//!   handing it input and it no longer counts toward [`MAX_ENCODERS`].
//! - The encoders' queues hold frames the stream's queue no longer does. [`Split::held`] counts
//!   them, so the compositor keeps enough frames for every encoder's queue to fill.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::thread::JoinHandle;

use craft_packet::MAX_SUBSCRIBERS;
use crossbeam_queue::ArrayQueue;
use studiocraft_media::Tick;
use studiocraft_media::audio::{AudioBlock, AudioPool, BLOCK_SAMPLES, PooledBlock};
use studiocraft_media::pool::Pool;
use studiocraft_media::queue::Fifo;
use studiocraft_media::video::PooledFrame;
use studiocraft_plugin::PluginError;

use crate::writer::IDLE;

/// Blocks of sound an encoder's pool holds beyond its queue: the block it is encoding, and one
/// being copied.
const SPARE_BLOCKS: usize = 2;
/// The value of [`Split::newest`] before the first frame.
const NO_FRAME: i64 = i64::MIN;
/// Most encoders a split feeds: the stream's, and one for each destination a stream can have.
pub(crate) const MAX_ENCODERS: usize = MAX_SUBSCRIBERS + 1;

/// The input queues of one video encoder.
pub(crate) struct Input {
    pub(crate) video: Arc<Fifo<Arc<PooledFrame>>>,
    pub(crate) audio: Arc<Fifo<PooledBlock>>,
}

/// The input queues of one video encoder as the split holds them: weakly, so dropping the encoder
/// ends them.
struct Fed {
    video: Weak<Fifo<Arc<PooledFrame>>>,
    audio: Weak<Fifo<PooledBlock>>,
    /// The blocks its copies of the sound are made in.
    pool: Arc<AudioPool>,
}

impl Fed {
    fn of(input: &Input) -> Fed {
        let pool = Pool::new(input.audio.capacity() + SPARE_BLOCKS, silent_block);
        Fed { video: Arc::downgrade(&input.video), audio: Arc::downgrade(&input.audio), pool }
    }

    /// Whether the encoder still holds its queues.
    fn alive(&self) -> bool {
        self.video.strong_count() > 0
    }
}

/// The thread that hands a stream's input to each of its encoders.
pub(crate) struct Split {
    video: Arc<Fifo<Arc<PooledFrame>>>,
    audio: Arc<Fifo<PooledBlock>>,
    /// Encoders added while the stream runs, for the thread to pick up.
    joins: Arc<ArrayQueue<Fed>>,
    /// The video queue of every encoder fed, the first one included, for the count of those
    /// still held.
    fed: Mutex<Vec<Weak<Fifo<Arc<PooledFrame>>>>>,
    /// The frames the encoders of `fed` still held when the last one was added can hold.
    held: AtomicUsize,
    /// The presentation time of the newest frame handed on, as ticks; [`NO_FRAME`] before one.
    newest: Arc<AtomicI64>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// How many frames the encoders of `fed` still held can hold at once: each one's queue, and the
/// frame it has taken.
fn holds(fed: &[Weak<Fifo<Arc<PooledFrame>>>]) -> usize {
    fed.iter().filter_map(Weak::upgrade).map(|v| v.capacity() + 1).sum()
}

fn silent_block() -> AudioBlock {
    AudioBlock { samples: vec![0.0; BLOCK_SAMPLES], pts: Tick(0) }
}

impl Split {
    /// Start handing the stream's input to `first`, with queues as large as `first`'s, for as long
    /// as its encoder holds them.
    pub(crate) fn start(first: &Input) -> Result<Split, PluginError> {
        let video = Arc::new(Fifo::new(first.video.capacity()));
        let audio = Arc::new(Fifo::new(first.audio.capacity()));
        let joins = Arc::new(ArrayQueue::new(MAX_ENCODERS));
        let newest = Arc::new(AtomicI64::new(NO_FRAME));
        let stop = Arc::new(AtomicBool::new(false));
        let (v, a, j, n, s) = (Arc::clone(&video), Arc::clone(&audio), Arc::clone(&joins), Arc::clone(&newest), Arc::clone(&stop));
        let fed = vec![Arc::downgrade(&first.video)];
        let held = AtomicUsize::new(holds(&fed));
        let fed = Mutex::new(fed);
        let first = Fed::of(first);
        let thread = std::thread::Builder::new()
            .name("stream split".into())
            .spawn(move || run(&v, &a, &j, &n, &s, first))
            .map_err(|e| PluginError::Failed(format!("no thread to share the stream's frames between its encoders: {e}")))?;
        Ok(Split { video, audio, joins, fed, held, newest, stop, thread: Some(thread) })
    }

    /// The queue the stream's frames go into.
    pub(crate) fn video(&self) -> Arc<Fifo<Arc<PooledFrame>>> {
        Arc::clone(&self.video)
    }

    /// The queue the stream's sound goes into.
    pub(crate) fn audio(&self) -> Arc<Fifo<PooledBlock>> {
        Arc::clone(&self.audio)
    }

    /// Hand the input to `input` too, from the next frame on, for as long as the encoder holds
    /// its queues. An error when the split already feeds [`MAX_ENCODERS`] encoders that are held:
    /// the stream's and one for each destination it can have.
    pub(crate) fn add(&self, input: &Input) -> Result<(), PluginError> {
        let refused = || PluginError::Failed(format!("a stream can run at most {MAX_ENCODERS} video encoders"));
        let mut fed = self.fed.lock().unwrap_or_else(PoisonError::into_inner);
        fed.retain(|v| v.strong_count() > 0);
        if fed.len() >= MAX_ENCODERS {
            return Err(refused());
        }
        // Full only while the thread has not yet picked up as many encoders as the count allows,
        // dropped ones included. Those are taken out, so the encoders held always fit. Only `add`
        // pushes, with `fed` held, so the room made stays free.
        if let Err(back) = self.joins.push(Fed::of(input)) {
            for _ in 0..self.joins.len() {
                if let Some(waiting) = self.joins.pop().filter(Fed::alive) {
                    let _ = self.joins.push(waiting);
                }
            }
            self.joins.push(back).map_err(|_| refused())?;
        }
        fed.push(Arc::downgrade(&input.video));
        self.held.store(holds(&fed), Ordering::Release);
        Ok(())
    }

    /// How many frames the encoders fed can hold at once: each one's queue, and the frame it has
    /// taken. They can all be frames the stream's queue no longer holds, when the encoders fall
    /// behind at different rates. Counted as each encoder is added, so an encoder dropped since
    /// the last was added still counts. Read without allocating.
    pub(crate) fn held(&self) -> usize {
        self.held.load(Ordering::Acquire)
    }

    /// Frames the stream's video queue dropped because it was full; every encoder lost them.
    pub(crate) fn dropped(&self) -> u64 {
        self.video.dropped()
    }

    /// The presentation time of the newest frame handed on; `None` before the first.
    pub(crate) fn newest(&self) -> Option<Tick> {
        let t = self.newest.load(Ordering::Acquire);
        (t != NO_FRAME).then_some(Tick(t))
    }

    /// Hand on what the queues hold and end the thread.
    pub(crate) fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Split {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(video: &Fifo<Arc<PooledFrame>>, audio: &Fifo<PooledBlock>, joins: &ArrayQueue<Fed>, newest: &AtomicI64, stop: &AtomicBool, first: Fed) {
    let mut outs = Vec::with_capacity(MAX_ENCODERS);
    outs.push(first);
    // The sound queues of the encoders a block goes to, with their pools, kept between blocks.
    let mut sound = Vec::with_capacity(MAX_ENCODERS);
    loop {
        // Read before the queues, so what was queued before the stop is still handed on.
        let stopping = stop.load(Ordering::SeqCst);
        let mut idle = true;
        // An encoder is picked up after each item is taken, so one added before an item was
        // queued gets that item. At most a queue's worth of each is taken in a turn.
        for _ in 0..video.capacity() {
            let Some(frame) = video.pop() else { break };
            idle = false;
            join(joins, &mut outs);
            newest.store(frame.pts.0, Ordering::Release);
            for q in outs.iter().filter_map(|o| o.video.upgrade()) {
                q.push(Arc::clone(&frame));
            }
        }
        for _ in 0..audio.capacity() {
            let Some(block) = audio.pop() else { break };
            idle = false;
            join(joins, &mut outs);
            sound.extend(outs.iter().filter_map(|o| Some((o.audio.upgrade()?, Arc::clone(&o.pool)))));
            hand_on(block, &sound);
            sound.clear();
        }
        // A queue holds at most a turn's worth, so this turn handed on everything queued before
        // the stop, and input that keeps coming does not hold the stop up.
        if stopping {
            return;
        }
        if idle {
            std::thread::sleep(IDLE);
        }
    }
}

/// Drops the encoders of `outs` that were dropped, and adds those waiting in `joins`.
fn join(joins: &ArrayQueue<Fed>, outs: &mut Vec<Fed>) {
    outs.retain(Fed::alive);
    while let Some(fed) = joins.pop() {
        outs.push(fed);
    }
}

/// A copy of `block` from its pool to every queue of `queues` but the last, and `block` to the
/// last.
fn hand_on(block: PooledBlock, queues: &[(Arc<Fifo<PooledBlock>>, Arc<AudioPool>)]) {
    let Some(((last, _), rest)) = queues.split_last() else { return };
    for (q, pool) in rest {
        let mut copy = pool.take();
        copy.samples.clear();
        copy.samples.extend_from_slice(&block.samples);
        copy.pts = block.pts;
        q.push(copy);
    }
    last.push(block);
}

#[cfg(test)]
mod tests {
    use studiocraft_media::video::VideoFrame;

    use super::*;

    /// The queues of an encoder that hold `n` items each.
    fn input_of(n: usize) -> Input {
        Input { video: Arc::new(Fifo::new(n)), audio: Arc::new(Fifo::new(n)) }
    }

    fn input() -> Input {
        input_of(8)
    }

    fn frame(frames: &Arc<Pool<VideoFrame>>, pts: i64) -> Arc<PooledFrame> {
        let mut f = frames.take();
        f.pts = Tick(pts);
        Arc::new(f)
    }

    fn block(blocks: &Arc<AudioPool>, pts: i64, value: f32) -> PooledBlock {
        let mut b = blocks.take();
        b.samples.clear();
        b.samples.resize(BLOCK_SAMPLES, value);
        b.pts = Tick(pts);
        b
    }

    #[test]
    fn every_encoder_gets_each_frame_and_its_own_copy_of_the_sound() {
        let (a, b) = (input(), input());
        let (av, aa, bv, ba) = (Arc::clone(&a.video), Arc::clone(&a.audio), Arc::clone(&b.video), Arc::clone(&b.audio));
        let mut split = Split::start(&a).unwrap();
        assert_eq!(split.newest(), None);
        split.add(&b).unwrap();
        let frames: Arc<Pool<VideoFrame>> = Pool::new(4, VideoFrame::default);
        let blocks: Arc<AudioPool> = Pool::new(4, silent_block);
        // The added encoder is picked up before the next input, and the stop hands on what was
        // queued before it returns.
        split.video().push(frame(&frames, 1));
        split.audio().push(block(&blocks, 7, 0.25));
        split.stop();
        assert_eq!((av.len(), bv.len(), aa.len(), ba.len()), (1, 1, 1, 1));
        let (fa, fb) = (av.pop().unwrap(), bv.pop().unwrap());
        assert!(Arc::ptr_eq(&fa, &fb), "a frame is shared, not copied");
        assert_eq!(split.newest(), Some(Tick(1)));
        let (sa, sb) = (aa.pop().unwrap(), ba.pop().unwrap());
        assert_eq!((sa.pts, sb.pts), (Tick(7), Tick(7)));
        assert!(sa.samples.iter().chain(sb.samples.iter()).all(|s| *s == 0.25));
        assert_eq!((sa.samples.len(), sb.samples.len()), (BLOCK_SAMPLES, BLOCK_SAMPLES));
    }

    #[test]
    fn an_encoder_that_falls_behind_costs_only_itself_frames() {
        // The stream's queues are as large as the first encoder's, so they hold every frame.
        let (quick, slow) = (input_of(32), input());
        let (qv, sv) = (Arc::clone(&quick.video), Arc::clone(&slow.video));
        let mut split = Split::start(&quick).unwrap();
        split.add(&slow).unwrap();
        let frames: Arc<Pool<VideoFrame>> = Pool::new(32, VideoFrame::default);
        // Neither encoder takes a frame: the quick one has room for all, the slow one for 8.
        for pts in 0..20 {
            split.video().push(frame(&frames, pts));
        }
        split.stop();
        assert_eq!((qv.len(), qv.dropped()), (20, 0), "the quick encoder got every frame");
        assert_eq!(sv.len(), sv.capacity(), "the slow encoder's queue is full");
        assert_eq!(sv.dropped(), 20 - sv.capacity() as u64, "and its oldest frames were dropped");
        assert_eq!(sv.pop().map(|f| f.pts), Some(Tick(20 - sv.capacity() as i64)), "it kept the newest");
    }

    #[test]
    fn a_stop_hands_on_what_was_queued_and_ends_the_thread() {
        let a = input();
        let (av, aa) = (Arc::clone(&a.video), Arc::clone(&a.audio));
        let mut split = Split::start(&a).unwrap();
        let frames: Arc<Pool<VideoFrame>> = Pool::new(4, VideoFrame::default);
        let blocks: Arc<AudioPool> = Pool::new(4, silent_block);
        split.video().push(frame(&frames, 3));
        split.audio().push(block(&blocks, 3, 0.5));
        split.stop();
        assert_eq!((av.len(), aa.len()), (1, 1));
        assert!(split.thread.is_none());
        // A second stop, and the drop after it, do nothing more.
        split.stop();
    }

    #[test]
    fn no_more_encoders_join_than_a_stream_can_have_destinations() {
        let first = input();
        let split = Split::start(&first).unwrap();
        // The stream's encoder is the first; one more can join for each destination.
        let mut held: Vec<Input> = (1..MAX_ENCODERS).map(|_| input()).collect();
        for (i, a) in held.iter().enumerate() {
            assert!(split.add(a).is_ok(), "encoder {}", i + 2);
        }
        let one_more = input();
        let Err(PluginError::Failed(e)) = split.add(&one_more) else { panic!("one encoder too many") };
        assert_eq!(e, format!("a stream can run at most {MAX_ENCODERS} video encoders"));
        assert_eq!(MAX_ENCODERS, MAX_SUBSCRIBERS + 1);
        // An encoder that was dropped no longer counts, so another can take its place.
        drop(held.pop());
        assert!(split.add(&one_more).is_ok());
        let Err(PluginError::Failed(_)) = split.add(&input()) else { panic!("one encoder too many") };
    }

    #[test]
    fn the_frames_held_count_every_encoders_queue_and_the_frame_it_took() {
        let first = input_of(4);
        let split = Split::start(&first).unwrap();
        assert_eq!(split.held(), 5);
        let (late, later) = (input_of(16), input_of(8));
        split.add(&late).unwrap();
        assert_eq!(split.held(), 5 + 17, "an encoder added while the stream runs counts");
        drop(late);
        assert_eq!(split.held(), 22, "a dropped encoder counts until the next is added");
        split.add(&later).unwrap();
        assert_eq!(split.held(), 5 + 9);
    }

    #[test]
    fn encoders_dropped_before_the_thread_picked_them_up_leave_room_for_others() {
        let first = input();
        let split = Split::start(&first).unwrap();
        // No input comes, so the thread picks up none of the encoders added.
        let mut held: Vec<Input> = (1..MAX_ENCODERS).map(|_| input()).collect();
        for a in &held {
            split.add(a).unwrap();
        }
        // Each one dropped leaves room for one more, however many were dropped.
        for i in 0..3 {
            drop(held.pop());
            let again = input();
            assert!(split.add(&again).is_ok(), "replacement {}", i + 1);
            held.push(again);
        }
        let Err(PluginError::Failed(_)) = split.add(&input()) else { panic!("one encoder too many") };
    }

    #[test]
    fn copies_of_the_sound_come_from_each_encoders_pool_without_allocating_once_warm() {
        let inputs: Vec<Input> = (0..3).map(|_| input()).collect();
        let fed: Vec<Fed> = inputs.iter().map(Fed::of).collect();
        let queues: Vec<_> = inputs.iter().zip(&fed).map(|(i, f)| (Arc::clone(&i.audio), Arc::clone(&f.pool))).collect();
        let blocks: Arc<AudioPool> = Pool::new(4, silent_block);
        // No encoder takes the sound, so every queue fills and drops its oldest block.
        for pts in 0..100 {
            hand_on(block(&blocks, pts, 0.5), &queues);
        }
        assert!(inputs.iter().all(|i| i.audio.len() == i.audio.capacity()));
        // The last encoder gets the blocks themselves; the others get copies from their own pools.
        for f in &fed[..2] {
            assert!(f.pool.made() <= f.pool.capacity(), "{} blocks made", f.pool.made());
            assert_eq!(f.pool.capacity(), 8 + SPARE_BLOCKS);
        }
    }

    #[test]
    fn an_encoder_that_was_dropped_gets_nothing_more() {
        let (a, b) = (input(), input());
        let (av, bv) = (Arc::clone(&a.video), Arc::downgrade(&b.video));
        let mut split = Split::start(&a).unwrap();
        split.add(&b).unwrap();
        drop(b);
        assert!(bv.upgrade().is_none(), "the split holds the encoder's queues weakly");
        let frames: Arc<Pool<VideoFrame>> = Pool::new(4, VideoFrame::default);
        split.video().push(frame(&frames, 1));
        split.video().push(frame(&frames, 2));
        split.stop();
        assert_eq!(av.len(), 2, "the encoder that is held still gets every frame");
        assert_eq!(split.newest(), Some(Tick(2)));
    }
}
