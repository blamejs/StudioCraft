//! What the PipeWire captures on Linux share: starting the library, and stopping a capture's loop
//! from another thread.

use std::cell::Cell;
use std::io::PipeReader;
use std::rc::Rc;
use std::sync::Once;

use pipewire as pw;
use pw::spa::support::system::IoFlags;

pub fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(pw::init);
}

/// Quit `mainloop` once the other end of `stop` closes, setting `stopping` first. Nothing is written
/// to the pipe; closing it makes it readable.
pub fn watch_stop<'l>(mainloop: &'l pw::main_loop::MainLoopRc, stop: PipeReader, stopping: Rc<Cell<bool>>) -> pw::loop_::IoSource<'l, PipeReader> {
    let quit = mainloop.clone();
    mainloop.loop_().add_io(stop, IoFlags::IN | IoFlags::HUP | IoFlags::ERR, move |_| {
        stopping.set(true);
        quit.quit();
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closing_the_stop_pipe_ends_the_capture_loop() {
        init();
        // A loop runs without a server, so this works on build machines too.
        let mainloop = pw::main_loop::MainLoopRc::new(None).unwrap();
        let (reader, writer) = std::io::pipe().unwrap();
        let stopping = Rc::new(Cell::new(false));
        let _watch = watch_stop(&mainloop, reader, Rc::clone(&stopping));
        drop(writer);
        mainloop.run();
        assert!(stopping.get(), "the loop stopped because the pipe closed");
    }
}
