//! Ending threads without making their owner wait: a thread that is asked to stop is handed to
//! one reaper thread, which joins it once it has ended. A thread stuck in a system call does not
//! hold up the others.

use std::sync::OnceLock;
use std::sync::mpsc::{RecvTimeoutError, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

/// Threads the reaper keeps; past this a handed-over thread is detached instead.
const PENDING: usize = 32;
/// How often the reaper looks for threads that have ended.
const CHECK: Duration = Duration::from_secs(1);

fn reaper() -> Option<&'static SyncSender<JoinHandle<()>>> {
    static TX: OnceLock<Option<SyncSender<JoinHandle<()>>>> = OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = sync_channel::<JoinHandle<()>>(PENDING);
        let spawned = std::thread::Builder::new().name("app audio reaper".into()).spawn(move || {
            let mut waiting: Vec<JoinHandle<()>> = Vec::with_capacity(PENDING);
            loop {
                match rx.recv_timeout(CHECK) {
                    // Past the limit the handle is dropped, which detaches the thread.
                    Ok(handle) if waiting.len() < PENDING => waiting.push(handle),
                    Ok(_) | Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                let mut i = 0;
                while i < waiting.len() {
                    if waiting.get(i).is_some_and(JoinHandle::is_finished) {
                        let _ = waiting.swap_remove(i).join();
                    } else {
                        i += 1;
                    }
                }
            }
        });
        spawned.ok().map(|_| tx)
    })
    .as_ref()
}

/// Hand `handle` over to be joined off this thread once it ends. It never blocks: when the reaper
/// is busy or could not start, the thread is detached and ends on its own.
pub(crate) fn reap(handle: JoinHandle<()>) {
    if let Some(tx) = reaper() {
        let _ = tx.try_send(handle);
    }
}
