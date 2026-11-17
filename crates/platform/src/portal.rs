//! Display and window capture on Linux through the desktop's screen-cast portal.
//!
//! Opening a capture starts a thread that asks the portal for a display or a window. The desktop
//! shows its own picker; the capture waits without blocking the caller. The portal then hands over
//! a PipeWire stream, which a second thread reads, copying each picture into a spare frame of a
//! [`FrameSlot`]. The portal thread waits for the capture to be dropped, the desktop to close the
//! session, or the stream to end; it then closes a pipe the stream thread's loop watches and
//! closes the portal session.

use std::cell::Cell;
use std::io::PipeReader;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::Arc;

use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, ResponseError};
use futures_channel::oneshot;
use futures_util::future::{Either, select};
use futures_util::{FutureExt, StreamExt};
use pipewire as pw;
use pw::spa;
use studiocraft_media::video::VideoFrame;

use crate::PlatformError;
use crate::pw_loop::{init, watch_stop};
use crate::screen_frames::{FrameSlot, Layout, State, copy_frame};

/// What the picker offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Display,
    Window,
}

/// The largest picture side the stream accepts, the same as a frame's.
const MAX_SIDE: u32 = 8192;

fn could_not_start() -> PlatformError {
    PlatformError::Failed("the screen capture could not start".into())
}

/// A display or window capture through the portal.
pub struct PortalCapture {
    slot: Arc<FrameSlot>,
    /// Dropping this wakes the portal thread, which then stops the stream and closes the session.
    _cancel: oneshot::Sender<()>,
}

impl PortalCapture {
    /// Start asking the portal for a display or window. Returns at once; the picture arrives once
    /// a choice has been made in the system picker.
    pub fn open(kind: Kind) -> Result<PortalCapture, PlatformError> {
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let slot = Arc::new(FrameSlot::default());
        let shared = Arc::clone(&slot);
        // The thread is not joined; it ends on its own once the capture is dropped.
        std::thread::Builder::new()
            .name("screen capture portal".into())
            .spawn(move || {
                if pollster::block_on(capture(kind, &shared, cancel_rx)).is_err() {
                    shared.set_state(State::Failed);
                }
            })
            .map_err(|_| could_not_start())?;
        Ok(PortalCapture { slot, _cancel: cancel_tx })
    }

    /// The newest picture into `frame`.
    pub fn grab(&mut self, frame: &mut VideoFrame) -> Result<(), PlatformError> {
        if self.slot.take(frame) {
            return Ok(());
        }
        match self.slot.state() {
            State::Waiting => Err(PlatformError::Waiting),
            // The desktop sends a picture only when the screen changes.
            State::Streaming => Err(PlatformError::Unchanged),
            State::Canceled => Err(PlatformError::Stopped("the choice in the system picker was canceled".into())),
            State::Ended => Err(PlatformError::Stopped("sharing stopped".into())),
            State::Failed => Err(PlatformError::Failed("the screen capture failed".into())),
            State::NoPortal => Err(PlatformError::Failed("the desktop has no screen-cast portal".into())),
        }
    }
}

/// Ask the portal for a stream, read it on a thread of its own, and close the session once the
/// capture is dropped, the desktop closes the session, or the stream ends. A capture dropped while
/// the request is pending closes the session, which ends the request.
async fn capture(kind: Kind, slot: &Arc<FrameSlot>, cancel: oneshot::Receiver<()>) -> Result<(), ashpd::Error> {
    let Ok(proxy) = Screencast::new().await else {
        slot.set_state(State::NoPortal);
        return Ok(());
    };
    let session = proxy.create_session(Default::default()).await?;
    // The desktop closes the session when sharing is stopped from its own controls.
    let closed = session.receive_closed().await?;
    let mut closed = std::pin::pin!(closed);
    let cursor = match proxy.available_cursor_modes().await {
        Ok(modes) if modes.contains(CursorMode::EMBEDDED) => CursorMode::EMBEDDED,
        _ => CursorMode::HIDDEN,
    };
    let sources = match kind {
        Kind::Display => SourceType::MONITOR,
        Kind::Window => SourceType::WINDOW,
    };
    let options =
        SelectSourcesOptions::default().set_cursor_mode(cursor).set_sources(sources).set_multiple(false).set_persist_mode(PersistMode::DoNot);
    let picked = async {
        proxy.select_sources(&session, options).await?.response()?;
        let streams = proxy.start(&session, None, Default::default()).await?.response()?;
        // One source was asked for; a portal that returns none has nothing to show.
        let node = streams.streams().first().map(|s| s.pipe_wire_node_id());
        let fd = proxy.open_pipe_wire_remote(&session, Default::default()).await?;
        Ok::<_, ashpd::Error>((node, fd))
    };
    let (picked, cancel) = match select(std::pin::pin!(picked), cancel).await {
        Either::Left(both) => both,
        Either::Right(_) => {
            let _ = session.close().await;
            return Ok(());
        }
    };
    let result = match picked {
        Err(ashpd::Error::Response(ResponseError::Cancelled)) => {
            slot.set_state(State::Canceled);
            Ok(())
        }
        Err(e) => Err(e),
        Ok((None, _)) => {
            slot.set_state(State::Failed);
            Ok(())
        }
        Ok((Some(node), fd)) => {
            follow_stream(&proxy, node, fd, slot, cancel, closed.next()).await;
            Ok(())
        }
    };
    let _ = session.close().await;
    result
}

/// Read the stream on its own thread until the capture is dropped (`cancel`), the desktop closes
/// the session (`closed`), or the stream ends; then stop the stream thread.
async fn follow_stream(
    proxy: &Screencast,
    node: u32,
    fd: OwnedFd,
    slot: &Arc<FrameSlot>,
    cancel: oneshot::Receiver<()>,
    closed: impl Future + Unpin,
) {
    let Ok((stop_rx, stop_tx)) = std::io::pipe() else {
        slot.set_state(State::Failed);
        return;
    };
    // The stream thread reports whether its stream failed; the state is set here, so that a
    // session the desktop closes counts as stopped sharing whichever of the two arrives first.
    let (done_tx, done_rx) = oneshot::channel::<bool>();
    let shared = Arc::clone(slot);
    let spawned = std::thread::Builder::new().name("screen capture".into()).spawn(move || {
        let _ = done_tx.send(read_stream(node, fd, &shared, stop_rx).unwrap_or(true));
    });
    if spawned.is_err() {
        slot.set_state(State::Failed);
        return;
    }
    match select(cancel, select(closed, done_rx)).await {
        Either::Left(_) => {}
        // Sharing was stopped from the desktop.
        Either::Right((Either::Left(_), _)) => slot.set_state(State::Ended),
        Either::Right((Either::Right((failed, closed)), _)) => {
            if failed.unwrap_or(true) {
                // The portal sends Closed before the reply to a later call, so a session it
                // closed while the stream went down is seen after this round trip.
                let _ = proxy.available_cursor_modes().await;
                slot.set_state(if closed.now_or_never().is_some() { State::Ended } else { State::Failed });
            }
        }
    }
    drop(stop_tx);
}

/// The pixel layout of a stream format, or `None` for a format the capture does not read.
fn layout(format: spa::param::video::VideoFormat) -> Option<Layout> {
    use spa::param::video::VideoFormat;
    match format {
        VideoFormat::RGBA => Some(Layout::Rgba),
        VideoFormat::RGBx => Some(Layout::Rgbx),
        VideoFormat::BGRA => Some(Layout::Bgra),
        VideoFormat::BGRx => Some(Layout::Bgrx),
        _ => None,
    }
}

/// The formats the stream offers to take: 4-byte pixels in memory the capture can map.
fn format_param() -> Result<Vec<u8>, PlatformError> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    let object = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: MAX_SIDE, height: MAX_SIDE }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            spa::utils::Fraction { num: 30, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 1000, denom: 1 }
        ),
    );
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(object))
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|_| could_not_start())
}

/// What the stream's callbacks share on the capture thread.
struct StreamData {
    slot: Arc<FrameSlot>,
    layout: Option<Layout>,
    width: u32,
    height: u32,
    connected: bool,
}

/// Read the portal's PipeWire stream `node` through `fd` until the pipe closes or the stream ends.
/// Returns whether the stream failed or lost its server connection.
fn read_stream(node: u32, fd: OwnedFd, slot: &Arc<FrameSlot>, stop: PipeReader) -> Result<bool, pw::Error> {
    init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_fd_rc(fd, None)?;
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
    };
    let stream = pw::stream::StreamBox::new(&core, "StudioCraft screen capture", props)?;
    let stopping = Rc::new(Cell::new(false));
    let failed = Rc::new(Cell::new(false));
    let data = StreamData { slot: Arc::clone(slot), layout: None, width: 0, height: 0, connected: false };
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .param_changed(|_, data, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = spa::param::video::VideoInfoRaw::new();
            if info.parse(param).is_err() {
                return;
            }
            // A format change, a resize included, takes effect with the next buffer.
            data.layout = layout(info.format());
            data.width = info.size().width;
            data.height = info.size().height;
        })
        .state_changed({
            let (mainloop, stopping, failed) = (mainloop.clone(), Rc::clone(&stopping), Rc::clone(&failed));
            move |_, data, _old, new| {
                if stopping.get() {
                    return;
                }
                match new {
                    pw::stream::StreamState::Error(_) => {
                        failed.set(true);
                        mainloop.quit();
                    }
                    // A lost server connection; sharing stopped from the desktop arrives as the
                    // session's Closed signal instead.
                    pw::stream::StreamState::Unconnected if data.connected => {
                        failed.set(true);
                        mainloop.quit();
                    }
                    pw::stream::StreamState::Unconnected => {}
                    pw::stream::StreamState::Connecting | pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
                        data.connected = true;
                    }
                }
            }
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(layout) = data.layout else { return };
            let Some(d) = buffer.datas_mut().first_mut() else { return };
            let (offset, size, stride) = (d.chunk().offset(), d.chunk().size(), d.chunk().stride());
            let Some(bytes) = d.data() else { return };
            let mut frame = data.slot.spare();
            if copy_frame(bytes, offset, size, stride, data.width, data.height, layout, &mut frame) {
                data.slot.publish(frame);
            }
        })
        .register()?;
    let param = format_param().map_err(|_| pw::Error::CreationFailed)?;
    let pod = spa::pod::Pod::from_bytes(&param).ok_or(pw::Error::CreationFailed)?;
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [pod],
    )?;
    let _stop = watch_stop(&mainloop, stop, stopping);
    mainloop.run();
    Ok(failed.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_takes_four_byte_formats_only() {
        use spa::param::video::VideoFormat;
        assert_eq!(layout(VideoFormat::BGRx), Some(Layout::Bgrx));
        assert_eq!(layout(VideoFormat::BGRA), Some(Layout::Bgra));
        assert_eq!(layout(VideoFormat::RGBx), Some(Layout::Rgbx));
        assert_eq!(layout(VideoFormat::RGBA), Some(Layout::Rgba));
        assert_eq!(layout(VideoFormat::RGB), None);
        assert_eq!(layout(VideoFormat::I420), None);
    }

    #[test]
    fn the_format_parameter_describes_a_pod() {
        let bytes = format_param().unwrap();
        assert!(spa::pod::Pod::from_bytes(&bytes).is_some());
    }
}
