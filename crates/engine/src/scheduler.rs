//! The frame scheduler: a thread that wakes once per frame on the master clock and runs the
//! compositor's frame. Frames the compositor could not make in time are skipped and counted, so
//! a slow frame never delays the ones after it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use studiocraft_media::FrameRate;
use studiocraft_media::clock::{MasterClock, PaceTick, Pacer};
use studiocraft_media::stats::RenderStats;

/// A running frame thread; dropping it stops the thread after its current frame.
pub struct Scheduler {
    stop: Arc<AtomicBool>,
}

impl Scheduler {
    /// Call `frame` at `rate` on `clock` on a new thread named `name`.
    pub fn spawn<F>(name: &str, clock: MasterClock, rate: FrameRate, stats: Arc<RenderStats>, mut frame: F) -> std::io::Result<Scheduler>
    where
        F: FnMut(PaceTick) + Send + 'static,
    {
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        std::thread::Builder::new().name(name.into()).spawn(move || {
            let mut pacer = Pacer::new(clock, rate);
            while !s.load(Ordering::Relaxed) {
                let t = pacer.wait();
                let start = Instant::now();
                frame(t);
                stats.record(start.elapsed(), t.skipped);
            }
        })?;
        Ok(Scheduler { stop })
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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
}
