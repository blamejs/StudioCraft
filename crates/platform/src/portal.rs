//! Display and window capture on Linux through the desktop's screen-cast portal.
//!
//! Opening a capture starts a thread that asks the portal for a display or a window. The desktop
//! shows its own picker; the capture waits without blocking the caller. The portal then hands over
//! a PipeWire stream, which a second thread reads, copying each picture into a spare frame of a
//! [`FrameSlot`]. The portal thread waits for the capture to be dropped, the desktop to close the
//! session, or the stream to end; it then closes a pipe the stream thread's loop watches and
//! closes the portal session.
//!
//! A display capture asked to stay on the GPU ([`PortalCapture::share_on_gpu`]) offers the stream
//! DMA-BUF buffers first, in the formats and DRM format modifiers the compositor published, and
//! buffers in memory after them. When the producer picks DMA-BUFs, each buffer's descriptor goes
//! into a ring the compositor imports, and the stream thread publishes shared frames. Each buffer
//! goes back to the producer once the compositor has finished reading its frame. Once the
//! compositor refuses the ring, or a buffer cannot be read, the stream renegotiates with the
//! memory formats only and the pictures are copied as before.

use std::cell::{Cell, RefCell};
use std::io::{PipeReader, Read};
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::sync::{Arc, Weak};

use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, ResponseError};
use craft_gpu_interop::{Dequeued, DmabufCaps, DmabufFormat, DmabufLayout, DmabufStream, HeldBuffers, MOD_LINEAR, SharedRing};
use futures_channel::oneshot;
use futures_util::future::{Either, select};
use futures_util::{FutureExt, StreamExt};
use pipewire as pw;
use pw::spa;
use pw::spa::support::system::IoFlags;
use studiocraft_media::video::VideoFrame;

use crate::PlatformError;
use crate::display::SharedGrab;
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
    kind: Kind,
    slot: Arc<FrameSlot>,
    /// The ring of the last picture on the GPU [`PortalCapture::grab_shared`] gave, and whether
    /// the stream thread was woken because the compositor refused it.
    last_ring: Weak<SharedRing>,
    woke: bool,
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
        Ok(PortalCapture { kind, slot, last_ring: Weak::new(), woke: false, _cancel: cancel_tx })
    }

    /// Ask for the display's pictures as DMA-BUFs that the compositor opens on the GPU, when the
    /// compositor has published what its device can import. It takes effect when the stream
    /// negotiates its format, so it is called before a choice is made in the picker. Returns
    /// whether it was asked for; a window capture always copies through memory.
    pub fn share_on_gpu(&mut self) -> bool {
        if self.kind != Kind::Display || craft_gpu_interop::dmabuf_caps().is_none() {
            return false;
        }
        self.slot.request_gpu();
        true
    }

    /// The newest picture on the GPU, without waiting. [`SharedGrab::Memory`] while the stream's
    /// buffers are in memory, which [`PortalCapture::grab`] copies, and before the first picture or
    /// once the stream has stopped or failed, which [`PortalCapture::grab`] reports. When the
    /// compositor has refused the last picture's ring, the stream thread is woken once to ask for
    /// pictures in memory, since a still screen sends no picture that would show it the refusal.
    pub fn grab_shared(&mut self) -> SharedGrab {
        match self.slot.take_shared() {
            Some(frame) => {
                self.last_ring = Arc::downgrade(frame.ring());
                self.woke = false;
                SharedGrab::Frame(frame)
            }
            None if self.slot.on_gpu() && self.slot.state() == State::Streaming => {
                if !self.woke && self.last_ring.upgrade().is_some_and(|r| r.fallback().is_some()) {
                    self.slot.wake_stream();
                    self.woke = true;
                }
                SharedGrab::Unchanged
            }
            None => SharedGrab::Memory,
        }
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
    let (Ok((stop_rx, stop_tx)), Ok((wake_rx, wake_tx))) = (std::io::pipe(), std::io::pipe()) else {
        slot.set_state(State::Failed);
        return;
    };
    slot.set_wake(wake_tx);
    // The stream thread reports whether its stream failed; the state is set here, so that a
    // session the desktop closes counts as stopped sharing whichever of the two arrives first.
    let (done_tx, done_rx) = oneshot::channel::<bool>();
    let shared = Arc::clone(slot);
    let spawned = std::thread::Builder::new().name("screen capture".into()).spawn(move || {
        let _ = done_tx.send(read_stream(node, fd, &shared, stop_rx, wake_rx).unwrap_or(true));
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

/// The stream format of a DMA-BUF format.
fn video_format(format: DmabufFormat) -> spa::param::video::VideoFormat {
    use spa::param::video::VideoFormat;
    match format {
        DmabufFormat::Bgrx => VideoFormat::BGRx,
        DmabufFormat::Bgra => VideoFormat::BGRA,
        DmabufFormat::Rgbx => VideoFormat::RGBx,
        DmabufFormat::Rgba => VideoFormat::RGBA,
    }
}

/// The DMA-BUF format of a stream format, or `None` for a format the capture does not import.
fn dmabuf_format(format: spa::param::video::VideoFormat) -> Option<DmabufFormat> {
    DmabufFormat::ALL.into_iter().find(|&f| video_format(f) == format)
}

/// The flag of a stream format that carries a DRM format modifier, and the one of a format whose
/// modifier the producer has not chosen yet (`spa/param/video/raw.h`).
const VIDEO_FLAG_MODIFIER: u32 = 1 << 2;
const VIDEO_FLAG_MODIFIER_FIXATION_REQUIRED: u32 = 1 << 3;

/// The flag of a property whose choices the producer fixates (`spa/pod/pod.h`).
const PROP_FLAG_DONT_FIXATE: u32 = 1 << 4;

/// The size and frame-rate ranges every offered format has.
fn size_and_rate() -> [spa::pod::Property; 2] {
    use spa::param::format::FormatProperties;
    [
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
    ]
}

fn serialize(object: spa::pod::Object) -> Result<Vec<u8>, PlatformError> {
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(object))
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|_| could_not_start())
}

/// The DMA-BUF formats the stream offers, one for each format `caps` imports with a modifier,
/// listing those modifiers for the producer to choose from.
fn dmabuf_params(caps: &DmabufCaps) -> Result<Vec<Vec<u8>>, PlatformError> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::pod::{ChoiceValue, Property, PropertyFlags, Value};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags};
    let mut out = Vec::new();
    for format in DmabufFormat::ALL {
        // Modifiers are 64-bit values that the pod carries as signed longs.
        let modifiers: Vec<i64> = caps.modifiers(format).iter().map(|&m| m as i64).collect();
        let Some(&first) = modifiers.first() else { continue };
        let modifier = Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY | PropertyFlags::from_bits_retain(PROP_FLAG_DONT_FIXATE),
            value: Value::Choice(ChoiceValue::Long(Choice(ChoiceFlags::empty(), ChoiceEnum::Enum { default: first, alternatives: modifiers }))),
        };
        let mut object = spa::pod::object!(
            spa::utils::SpaTypes::ObjectParamFormat,
            spa::param::ParamType::EnumFormat,
            spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            spa::pod::property!(FormatProperties::VideoFormat, Id, video_format(format))
        );
        object.properties.push(modifier);
        object.properties.extend(size_and_rate());
        out.push(serialize(object)?);
    }
    Ok(out)
}

/// The formats the stream offers to take: 4-byte pixels in memory the capture can map.
fn format_param() -> Result<Vec<u8>, PlatformError> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    let mut object = spa::pod::object!(
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
        )
    );
    object.properties.extend(size_and_rate());
    serialize(object)
}

/// The buffer kinds the stream takes for the negotiated format: DMA-BUFs for a format with a DRM
/// format modifier, memory otherwise.
fn buffers_param(dmabuf: bool) -> Result<Vec<u8>, PlatformError> {
    use spa::buffer::DataType;
    use spa::pod::{Property, PropertyFlags, Value};
    let types = if dmabuf { 1 << DataType::DmaBuf.as_raw() } else { (1 << DataType::MemFd.as_raw()) | (1 << DataType::MemPtr.as_raw()) };
    serialize(spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![Property { key: spa::sys::SPA_PARAM_BUFFERS_dataType, flags: PropertyFlags::empty(), value: Value::Int(types) }],
    })
}

/// The formats a stream offers: DMA-BUFs first when `caps` is given, then memory.
fn offered_params(caps: Option<&DmabufCaps>) -> Result<Vec<Vec<u8>>, PlatformError> {
    let mut params = match caps {
        Some(caps) => dmabuf_params(caps)?,
        None => Vec::new(),
    };
    params.push(format_param()?);
    Ok(params)
}

/// Ask the stream for `params` again, in the order given.
fn renegotiate(stream: &pw::stream::Stream, params: &[Vec<u8>]) {
    let mut pods: Vec<&spa::pod::Pod> = params.iter().filter_map(|p| spa::pod::Pod::from_bytes(p)).collect();
    let _ = stream.update_params(&mut pods);
}

/// What the stream's callbacks share on the capture thread.
struct StreamData {
    slot: Arc<FrameSlot>,
    layout: Option<Layout>,
    width: u32,
    height: u32,
    connected: bool,
    /// What the compositor's device imports, while the stream may still offer DMA-BUFs.
    caps: Option<Arc<DmabufCaps>>,
    /// The DMA-BUF descriptors of the negotiated buffers, while the format has a modifier.
    dmabuf: DmabufStream,
    /// The DMA-BUF buffers kept from the producer until the compositor has read their frames. The
    /// stream thread's wake callback shares them ([`read_stream`]).
    held: Rc<RefCell<HeldBuffers>>,
    /// How many buffers the stream has.
    buffers: usize,
}

impl StreamData {
    /// The kept buffers. Every borrow ends within one callback of the stream thread, and none of
    /// them calls another, so the buffers are always free here.
    fn held(&self) -> Option<std::cell::RefMut<'_, HeldBuffers>> {
        self.held.try_borrow_mut().ok()
    }

    /// Stop offering DMA-BUFs and ask for the memory formats only, once. Buffers of the old format
    /// that arrive before the new one are dropped. A kept buffer whose frame no draw holds goes back
    /// now; one the GPU is still reading goes back once that draw has finished.
    fn leave_gpu(&mut self, stream: &pw::stream::Stream) {
        self.dmabuf.set_format(None);
        self.slot.set_on_gpu(false);
        let released = self.dmabuf.released();
        if let Some(mut held) = self.held() {
            held.release(stream, released);
        }
        if self.caps.take().is_some()
            && let Ok(param) = format_param()
        {
            renegotiate(stream, &[param]);
        }
    }
}

/// Read the portal's PipeWire stream `node` through `fd` until the `stop` pipe closes or the stream
/// ends. A byte on `wake` asks for pictures in memory ([`FrameSlot::wake_stream`]). Returns whether
/// the stream failed or lost its server connection.
fn read_stream(node: u32, fd: OwnedFd, slot: &Arc<FrameSlot>, stop: PipeReader, wake: PipeReader) -> Result<bool, pw::Error> {
    init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context.connect_fd_rc(fd, None)?;
    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
    };
    let stream = pw::stream::StreamRc::new(core.clone(), "StudioCraft screen capture", props)?;
    let stopping = Rc::new(Cell::new(false));
    let failed = Rc::new(Cell::new(false));
    let caps = if slot.gpu_requested() { craft_gpu_interop::dmabuf_caps() } else { None };
    let params = offered_params(caps.as_deref()).map_err(|_| pw::Error::CreationFailed)?;
    let held = Rc::new(RefCell::new(HeldBuffers::new()));
    let data = StreamData {
        slot: Arc::clone(slot),
        layout: None,
        width: 0,
        height: 0,
        connected: false,
        caps,
        dmabuf: DmabufStream::new(),
        held: Rc::clone(&held),
        buffers: 0,
    };
    let _listener = stream
        .add_local_listener_with_user_data(data)
        .param_changed(|stream, data, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let mut info = spa::param::video::VideoInfoRaw::new();
            if info.parse(param).is_err() {
                return;
            }
            let flags = info.flags().bits();
            // The producer, which allocates the buffers, has not chosen among the offered modifiers
            // yet. It fixes one and the chosen format follows; a consumer only reads that format.
            if flags & VIDEO_FLAG_MODIFIER_FIXATION_REQUIRED != 0 {
                return;
            }
            // A format change, a resize included, takes effect with the next buffer.
            data.layout = layout(info.format());
            data.width = info.size().width;
            data.height = info.size().height;
            let dmabuf = flags & VIDEO_FLAG_MODIFIER != 0;
            let gpu = match (&data.caps, dmabuf_format(info.format())) {
                (Some(caps), Some(format)) if dmabuf => {
                    Some((data.width, data.height, DmabufLayout { format, modifier: info.modifier() }, caps.device))
                }
                _ => None,
            };
            data.slot.set_on_gpu(gpu.is_some());
            data.dmabuf.set_format(gpu);
            if let Ok(param) = buffers_param(dmabuf) {
                renegotiate(stream, &[param]);
            }
        })
        .add_buffer(|_, data, _| {
            data.buffers = data.buffers.saturating_add(1);
            data.dmabuf.buffers_changed();
        })
        .remove_buffer(|_, data, buffer| {
            // A buffer the stream frees is never given back. Kept buffers the GPU may still read
            // stay allocated through the descriptors their ring duplicated.
            if let Some(mut held) = data.held() {
                held.forget(buffer as usize);
            }
            data.buffers = data.buffers.saturating_sub(1);
            data.dmabuf.buffers_changed();
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
            // The buffers whose frames the GPU has read, or whose frames were dropped undrawn, go
            // back to the producer.
            let released = data.dmabuf.released();
            if let Some(mut held) = data.held() {
                held.release(stream, released);
            }
            if data.dmabuf.active() && data.buffers < 2 {
                // A buffer kept while the GPU reads it would leave the producer none to write.
                data.leave_gpu(stream);
            }
            if data.dmabuf.active() {
                let Some(buffer) = Dequeued::take(stream) else { return };
                // A buffer in memory, though the stream took DMA-BUFs: it goes back unread, and
                // the stream asks for memory formats, whose pictures are copied.
                let Some(plane) = buffer.plane().filter(|p| p.dmabuf) else {
                    drop(buffer);
                    data.leave_gpu(stream);
                    return;
                };
                // A buffer whose chunk holds no whole picture goes back unread and is not shown, as
                // in memory. Producers send an empty chunk when the picture did not change.
                let modifier = data.dmabuf.layout().map_or(MOD_LINEAR, |l| l.modifier);
                if !dmabuf_chunk_holds_picture(plane.size, plane.stride, data.width, data.height, modifier) {
                    return;
                }
                // The descriptor is duplicated into the ring while the buffer is dequeued; the
                // compositor waits for the producer's writes on the duplicate. The chunk's offset
                // counts from where the buffer's data starts in the descriptor.
                let start = plane.mapoffset.checked_add(plane.offset);
                let shared = match (data.dmabuf.refused(), i32::try_from(plane.fd), u32::try_from(plane.stride), start) {
                    (Some(reason), ..) => Err(reason),
                    (None, Ok(fd), Ok(stride), Some(start)) => data.dmabuf.frame(fd, start, stride),
                    _ => Err(craft_gpu_interop::Fallback::ImportFailed),
                };
                match shared {
                    Ok(Some(frame)) => {
                        // The buffer stays the capture's until the GPU has read its frame, and the
                        // producer keeps at least one buffer to write into. A buffer that cannot be
                        // kept goes back unread, and its frame is not shown.
                        let limit = data.buffers.saturating_sub(1);
                        if data.held().is_some_and(|mut held| held.keep(buffer, &frame, limit)) {
                            data.slot.publish_shared(frame);
                        }
                    }
                    // A DMA-BUF the capture cannot use: one it was refused, one it cannot read, or
                    // one in a format it did not take as a DMA-BUF format.
                    Ok(None) | Err(_) => {
                        drop(buffer);
                        data.leave_gpu(stream);
                    }
                }
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(d) = buffer.datas_mut().first_mut() else { return };
            let (offset, size, stride) = (d.chunk().offset(), d.chunk().size(), d.chunk().stride());
            if d.type_() == spa::buffer::DataType::DmaBuf {
                // A DMA-BUF in a format the capture did not take as a DMA-BUF format.
                drop(buffer);
                data.leave_gpu(stream);
                return;
            }
            let Some(layout) = data.layout else { return };
            let Some(bytes) = d.data() else { return };
            let mut frame = data.slot.spare();
            if copy_frame(bytes, offset, size, stride, data.width, data.height, layout, &mut frame) {
                data.slot.publish(frame);
            }
        })
        .register()?;
    let mut pods = params.iter().map(|p| spa::pod::Pod::from_bytes(p).ok_or(pw::Error::CreationFailed)).collect::<Result<Vec<_>, _>>()?;
    stream.connect(
        spa::utils::Direction::Input,
        Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut pods,
    )?;
    let _stop = watch_stop(&mainloop, stop, stopping);
    // The source thread saw the compositor refuse the stream's ring: the stream asks for memory
    // formats without waiting for its next picture. The slot keeps the pipe's other end open.
    let _wake = mainloop.loop_().add_io(wake, IoFlags::IN, {
        let (slot, stream) = (Arc::clone(slot), stream.downgrade());
        move |pipe: &mut PipeReader| {
            let _ = pipe.read(&mut [0; 16]);
            if slot.on_gpu()
                && let Some(stream) = stream.upgrade()
            {
                slot.set_on_gpu(false);
                // The frame not taken yet is gone, so its buffer, and any other buffer whose frame
                // was dropped undrawn, goes back now. A value of 0 gives back no buffer the GPU may
                // still be reading; those go back with the next picture's callback.
                if let Ok(mut held) = held.try_borrow_mut() {
                    held.release(&stream, 0);
                }
                if let Ok(param) = format_param() {
                    renegotiate(&stream, &[param]);
                }
            }
        }
    });
    mainloop.run();
    Ok(failed.get())
}

/// Whether a DMA-BUF chunk of `size` bytes holds a `width` x `height` picture with rows `stride`
/// bytes apart. An empty chunk holds none. Only a linear buffer's rows are laid out where the
/// chunk can be checked; it must reach the last row's pixels.
fn dmabuf_chunk_holds_picture(size: u32, stride: i32, width: u32, height: u32, modifier: u64) -> bool {
    if size == 0 || width == 0 || height == 0 {
        return false;
    }
    if modifier != MOD_LINEAR {
        return true;
    }
    let (Ok(stride), row) = (u64::try_from(stride), u64::from(width) * 4) else { return false };
    stride >= row && u64::from(size) >= u64::from(height - 1) * stride + row
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

    /// The properties of a serialized object, by key.
    fn properties(bytes: &[u8]) -> Vec<spa::pod::Property> {
        use spa::pod::deserialize::PodDeserializer;
        let (_, value) = PodDeserializer::deserialize_any_from(bytes).unwrap();
        let spa::pod::Value::Object(object) = value else { panic!("not an object") };
        object.properties
    }

    fn capture_with(cancel: oneshot::Sender<()>) -> PortalCapture {
        PortalCapture { kind: Kind::Display, slot: Arc::new(FrameSlot::default()), last_ring: Weak::new(), woke: false, _cancel: cancel }
    }

    #[test]
    fn a_refused_ring_wakes_the_stream_thread_once_without_waiting_for_a_picture() {
        let (cancel, _rx) = oneshot::channel();
        let mut cap = capture_with(cancel);
        let (mut woken, wake) = std::io::pipe().unwrap();
        cap.slot.set_wake(wake);
        cap.slot.set_on_gpu(true);
        let ring = SharedRing::detached(4, 4);
        let frame = |ready| craft_gpu_interop::SharedFrame::new(Arc::clone(&ring), 0, ready).unwrap();
        cap.slot.publish_shared(frame(1));
        assert!(matches!(cap.grab_shared(), SharedGrab::Frame(_)));
        // The screen stays still: no picture comes after the compositor refused the ring.
        assert!(matches!(cap.grab_shared(), SharedGrab::Unchanged));
        ring.refuse(craft_gpu_interop::Fallback::ImportFailed);
        assert!(matches!(cap.grab_shared(), SharedGrab::Unchanged));
        assert!(matches!(cap.grab_shared(), SharedGrab::Unchanged));
        // Dropping the capture closes the pipe's writing end, so reading it to the end never waits.
        drop(cap);
        let mut got = Vec::new();
        woken.read_to_end(&mut got).unwrap();
        assert_eq!(got, [1], "the stream thread was woken once");
    }

    #[test]
    fn only_a_dmabuf_chunk_that_reaches_the_last_row_holds_a_picture() {
        // 2 x 3 pixels, rows 16 bytes apart: the last row needs only its 8 bytes.
        assert!(dmabuf_chunk_holds_picture(40, 16, 2, 3, MOD_LINEAR));
        assert!(!dmabuf_chunk_holds_picture(39, 16, 2, 3, MOD_LINEAR));
        assert!(!dmabuf_chunk_holds_picture(0, 16, 2, 3, MOD_LINEAR), "an empty chunk carries no new picture");
        assert!(!dmabuf_chunk_holds_picture(40, 4, 2, 3, MOD_LINEAR), "rows shorter than the picture");
        assert!(!dmabuf_chunk_holds_picture(40, -16, 2, 3, MOD_LINEAR));
        assert!(!dmabuf_chunk_holds_picture(u32::MAX, 16, 0, 3, MOD_LINEAR));
        assert!(!dmabuf_chunk_holds_picture(u32::MAX, i32::MAX, u32::MAX, u32::MAX, MOD_LINEAR));
        // A tiled buffer's rows are not where the chunk says, so only an empty chunk is refused.
        assert!(dmabuf_chunk_holds_picture(1, 16, 2, 3, 5));
        assert!(!dmabuf_chunk_holds_picture(0, 16, 2, 3, 5));
    }

    #[test]
    fn a_gpu_capture_whose_stream_stopped_reports_it_through_memory() {
        let (cancel, _rx) = oneshot::channel();
        let mut cap = capture_with(cancel);
        cap.slot.set_on_gpu(true);
        // Before the first buffer the copy path reports that the capture is waiting.
        assert!(matches!(cap.grab_shared(), SharedGrab::Memory));
        cap.slot.set_state(State::Streaming);
        assert!(matches!(cap.grab_shared(), SharedGrab::Unchanged));
        for end in [State::Ended, State::Failed] {
            cap.slot.set_state(end);
            assert!(matches!(cap.grab_shared(), SharedGrab::Memory), "{end:?}");
            let mut frame = VideoFrame::default();
            assert!(matches!(cap.grab(&mut frame), Err(PlatformError::Stopped(_) | PlatformError::Failed(_))));
        }
    }

    #[test]
    fn every_dmabuf_format_maps_to_its_stream_format_and_back() {
        use spa::param::video::VideoFormat;
        for format in DmabufFormat::ALL {
            assert_eq!(dmabuf_format(video_format(format)), Some(format));
        }
        assert_eq!(dmabuf_format(VideoFormat::RGB), None);
        assert_eq!(dmabuf_format(VideoFormat::NV12), None);
    }

    #[test]
    fn the_stream_offers_one_dmabuf_format_per_importable_format_with_its_modifiers_before_memory() {
        use spa::param::format::FormatProperties;
        use spa::pod::{ChoiceValue, Value};
        use spa::utils::ChoiceEnum;
        let caps = DmabufCaps {
            device: craft_gpu_interop::AdapterId(7),
            formats: vec![(DmabufFormat::Bgrx, vec![5, 0]), (DmabufFormat::Rgba, vec![]), (DmabufFormat::Bgra, vec![0])],
        };
        let dmabuf = dmabuf_params(&caps).unwrap();
        // Rgba has no modifier the device reads, so it is not offered.
        assert_eq!(dmabuf.len(), 2);
        let first = properties(&dmabuf[0]);
        let format = first.iter().find(|p| p.key == FormatProperties::VideoFormat.as_raw()).unwrap();
        assert!(matches!(format.value, Value::Id(id) if id.0 == spa::param::video::VideoFormat::BGRx.as_raw()));
        let modifier = first.iter().find(|p| p.key == FormatProperties::VideoModifier.as_raw()).unwrap();
        assert_ne!(modifier.flags.bits() & PROP_FLAG_DONT_FIXATE, 0);
        let Value::Choice(ChoiceValue::Long(choice)) = &modifier.value else { panic!("not a choice of longs") };
        let ChoiceEnum::Enum { default, alternatives } = &choice.1 else { panic!("not an enum") };
        assert_eq!((*default, alternatives.as_slice()), (5, [5, 0].as_slice()));

        let offered = offered_params(Some(&caps)).unwrap();
        assert_eq!(offered.len(), 3);
        assert_eq!(offered[2], format_param().unwrap());
        assert!(properties(&offered[2]).iter().all(|p| p.key != FormatProperties::VideoModifier.as_raw()));
        // Without caps the stream offers memory only.
        assert_eq!(offered_params(None).unwrap(), vec![format_param().unwrap()]);
    }

    #[test]
    fn the_buffers_the_stream_takes_follow_the_negotiated_format() {
        use spa::buffer::DataType;
        use spa::pod::Value;
        let types = |dmabuf| {
            let found = properties(&buffers_param(dmabuf).unwrap()).into_iter().find(|p| p.key == spa::sys::SPA_PARAM_BUFFERS_dataType);
            match found.map(|p| p.value) {
                Some(Value::Int(types)) => types,
                _ => panic!("no data types"),
            }
        };
        assert_eq!(types(true), 1 << DataType::DmaBuf.as_raw());
        assert_eq!(types(false), (1 << DataType::MemFd.as_raw()) | (1 << DataType::MemPtr.as_raw()));
    }
}
