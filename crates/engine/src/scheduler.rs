//! The frame scheduler: a thread that wakes once per frame on the master clock and runs the
//! compositor's frame. Frames the compositor could not make in time are skipped and counted, so
//! a slow frame never delays the ones after it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use studiocraft_media::FrameRate;
use studiocraft_media::clock::{MasterClock, PaceTick, Pacer};
use studiocraft_media::stats::RenderStats;

/// A running frame thread; dropping it stops the thread after its current frame and waits for it
/// to end, so no frame is still drawing while the process exits.
pub struct Scheduler {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Scheduler {
    /// Call `frame` at `rate` on `clock` on a new thread named `name`.
    pub fn spawn<F>(name: &str, clock: MasterClock, rate: FrameRate, stats: Arc<RenderStats>, mut frame: F) -> std::io::Result<Scheduler>
    where
        F: FnMut(PaceTick) + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let thread = std::thread::Builder::new().name(name.into()).spawn(move || {
            let mut pacer = Pacer::new(clock, rate);
            while !s.load(Ordering::Relaxed) {
                let t = pacer.wait();
                if s.load(Ordering::Relaxed) {
                    break;
                }
                let start = Instant::now();
                frame(t);
                stats.record(start.elapsed(), t.skipped);
            }
        })?;
        Ok(Scheduler { stop, thread: Some(thread) })
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            // A frame that panicked has already ended the thread.
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    use super::*;

    #[test]
    fn runs_at_the_rate_and_counts_frames() {
        let stats = Arc::new(RenderStats::default());
        let n = Arc::new(AtomicU64::new(0));
        let n2 = Arc::clone(&n);
        let s = Scheduler::spawn("test frames", MasterClock::new(), FrameRate::new(100, 1), Arc::clone(&stats), move |_| {
            n2.fetch_add(1, Ordering::Relaxed);
        })
        .unwrap();
        std::thread::sleep(Duration::from_millis(250));
        drop(s);
        let frames = n.load(Ordering::Relaxed);
        assert!((10..=30).contains(&frames), "{frames}");
        assert_eq!(stats.frames.load(Ordering::Relaxed), frames);
    }

    #[test]
    fn dropping_waits_for_the_frame_thread_to_end() {
        let alive = Arc::new(());
        let held = Arc::clone(&alive);
        let (tx, rx) = std::sync::mpsc::channel();
        let s = Scheduler::spawn("test stop", MasterClock::new(), FrameRate::new(1000, 1), Arc::new(RenderStats::default()), move |_| {
            let _ = (&held, tx.send(()));
        })
        .unwrap();
        // A frame has run, so the thread is in its loop.
        rx.recv().unwrap();
        drop(s);
        // The thread has ended and dropped the frame closure with it.
        assert_eq!(Arc::strong_count(&alive), 1);
        while rx.try_recv().is_ok() {}
        assert_eq!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Disconnected));
    }
}
