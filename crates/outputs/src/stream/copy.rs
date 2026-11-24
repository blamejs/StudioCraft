//! A copy of a stream written to a Matroska file while the stream runs: the same encoded video and
//! sound the destinations get, for posting the stream as a video afterwards.
//!
//! - The copy takes the stream's packets as a destination does, from a ring of its own of the
//!   same size ([`super::RING_SECONDS`] seconds). It subscribes before the encoders start, so the
//!   file starts at the stream's first keyframe.
//! - A thread of its own creates the file and writes each packet in the order the destinations
//!   get them, with times counted from the first packet's decoding time, as a destination counts
//!   its timestamps. The file has the stream's video and sound tracks. The sound declares no
//!   encoder delay, since the copy's first sound packet comes after its first keyframe.
//! - A disk that falls more than the ring's length behind loses packets up to the next keyframe,
//!   as a destination does; [`StreamCopyState::lost_packets`] counts them. The encoders, the
//!   destinations and the other subscribers never wait for the copy.
//! - A file that cannot be created or written stops the copy and keeps what was written. The
//!   stream goes on.
//! - When the stream ends, the thread writes what its ring holds and finishes the file. A stop
//!   waits up to its limit for that, then leaves the file as it is; it plays up to its last whole
//!   cluster.

use std::fs::File;
use std::io::{BufWriter, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use craft_media::matroska::{MkvWriter, MuxOptions, TrackSpec};
use craft_packet::{Kind, Receiver, RecvError};
use studiocraft_media::stats::EncoderStats;
use studiocraft_plugin::StreamCopyState;

use super::{RECV_WAIT, lock, lost};
use crate::native::{Counted, Track, writing_app};

/// A file the copy can be written to.
pub(crate) trait CopyFile: Write + Seek + Send {}

impl<T: Write + Seek + Send> CopyFile for T {}

/// Opens the copy's file, on the copy's own thread.
pub(crate) type Open = Box<dyn FnOnce() -> std::io::Result<Box<dyn CopyFile>> + Send>;

/// How often a stop looks again whether the copy is finished.
const FINISH_POLL: Duration = Duration::from_millis(10);

/// A new file at `path`, written through a 1 MiB buffer.
pub(crate) fn create(path: &Path) -> Open {
    let path = path.to_path_buf();
    Box::new(move || {
        let file = File::create(&path)?;
        Ok(Box::new(BufWriter::with_capacity(1 << 20, file)) as Box<dyn CopyFile>)
    })
}

/// What the copy's thread and the stream share.
#[derive(Default)]
struct Shared {
    /// Bytes in the file.
    stats: Arc<EncoderStats>,
    lost: AtomicU64,
    finished: AtomicBool,
    /// Leave the file as it is: the thread ends without finishing it.
    abandon: AtomicBool,
    failure: Mutex<Option<String>>,
}

impl Shared {
    /// Record `message` unless an earlier failure is recorded.
    fn fail(&self, message: String) {
        let mut f = lock(&self.failure);
        if f.is_none() {
            *f = Some(message);
        }
    }
}

/// The running copy of a stream.
pub(crate) struct StreamCopy {
    path: PathBuf,
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl StreamCopy {
    /// Start writing the packets `rx` delivers to the file `open` makes, shown as `path`, with
    /// the stream's `tracks`. A thread that cannot be started is a failure of the copy only.
    pub(crate) fn start(path: PathBuf, open: Open, mut tracks: Vec<TrackSpec>, rx: Receiver) -> StreamCopy {
        if let Some(audio) = tracks.get_mut(Track::Audio as usize) {
            audio.codec_delay_ns = 0;
        }
        let shared = Arc::new(Shared::default());
        let (sh, shown) = (Arc::clone(&shared), path.display().to_string());
        let thread = std::thread::Builder::new().name("stream copy".into()).spawn(move || write(open, tracks, rx, &sh, &shown));
        let thread = match thread {
            Ok(t) => Some(t),
            Err(e) => {
                shared.fail(format!("the copy of the stream could not be started: {e}"));
                None
            }
        };
        StreamCopy { path, shared, thread }
    }

    /// A copy to `path` that could not start, for `message`.
    pub(crate) fn failed(path: PathBuf, message: String) -> StreamCopy {
        let shared = Arc::new(Shared::default());
        shared.fail(message);
        StreamCopy { path, shared, thread: None }
    }

    pub(crate) fn state(&self) -> StreamCopyState {
        StreamCopyState {
            path: self.path.clone(),
            bytes: self.shared.stats.bytes.load(Ordering::Relaxed),
            lost_packets: self.shared.lost.load(Ordering::Relaxed),
            finished: self.shared.finished.load(Ordering::SeqCst),
            failure: lock(&self.shared.failure).clone(),
        }
    }

    /// Wait up to `wait` for the copy to write what its ring holds and finish the file, once the
    /// stream's packets have ended; then leave a file that is not finished as it is.
    pub(crate) fn finish(&mut self, wait: Duration) {
        let Some(t) = self.thread.take() else { return };
        let deadline = Instant::now() + wait;
        while !t.is_finished() && Instant::now() < deadline {
            std::thread::sleep(FINISH_POLL);
        }
        if t.is_finished() {
            let _ = t.join();
            return;
        }
        self.shared.abandon.store(true, Ordering::SeqCst);
        self.shared.fail(format!("the copy was not finished in {} seconds; it plays up to the last part written", wait.as_secs()));
    }
}

impl Drop for StreamCopy {
    fn drop(&mut self) {
        // A thread still running ends at its next packet, or when its write returns.
        self.shared.abandon.store(true, Ordering::SeqCst);
    }
}

/// The copy's thread: create the file, write every packet `rx` delivers, and finish the file when
/// the stream's packets end.
fn write(open: Open, tracks: Vec<TrackSpec>, mut rx: Receiver, shared: &Shared, shown: &str) {
    let could_not = |e: &dyn std::fmt::Display| format!("the copy of the stream could not be written to {shown}: {e}");
    let file = match open() {
        Ok(f) => f,
        Err(e) => return shared.fail(could_not(&e)),
    };
    let opts = MuxOptions { writing_app: writing_app(), ..MuxOptions::default() };
    let mut mux = match MkvWriter::new(Counted::new(file, Arc::clone(&shared.stats)), tracks, opts) {
        Ok(m) => m,
        Err(e) => return shared.fail(could_not(&e)),
    };
    let mut zero = None;
    loop {
        if shared.abandon.load(Ordering::SeqCst) {
            // Dropping the writer writes out what it has buffered, without finishing the file.
            return;
        }
        let packet = match rx.recv_timeout(RECV_WAIT) {
            Ok(p) => p,
            Err(RecvError::Closed) => break,
            Err(RecvError::Timeout | RecvError::Empty) => continue,
        };
        let (track, keyframe) = match packet.kind() {
            Kind::Video => (Track::Video, packet.keyframe()),
            Kind::Audio => (Track::Audio, true),
            Kind::Data => continue,
        };
        // A time before the first packet is written at 0, as a destination sends it.
        let pts = packet.pts().saturating_sub(*zero.get_or_insert(packet.dts())).max(0);
        if let Err(e) = mux.write_frame(track as usize, pts, keyframe, packet.data(), None) {
            return shared.fail(could_not(&e));
        }
        shared.lost.store(lost(&rx), Ordering::Relaxed);
    }
    shared.lost.store(lost(&rx), Ordering::Relaxed);
    let not_finished = |e: &dyn std::fmt::Display| format!("the copy of the stream at {shown} could not be finished: {e}");
    let mut w = match mux.finish() {
        Ok(w) => w,
        Err(e) => return shared.fail(not_finished(&e)),
    };
    match w.flush() {
        Ok(()) => shared.finished.store(true, Ordering::SeqCst),
        Err(e) => shared.fail(not_finished(&e)),
    }
}
