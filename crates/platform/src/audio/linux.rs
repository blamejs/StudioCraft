//! Audio devices through PipeWire: the device list comes from the server's registry, and each
//! capture is a PipeWire stream on a thread of its own.

use std::cell::{Cell, RefCell};
use std::io::{PipeReader, PipeWriter};
use std::rc::Rc;
use std::sync::Once;
use std::sync::mpsc::{SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use pipewire as pw;
use pw::spa;
use pw::spa::support::system::IoFlags;
use pw::types::ObjectType;

use super::pw_props::{self, Node, StreamChange};
use super::{DEFAULT_DEVICE, DeviceInfo, DeviceKind, Fill, Format, OnError};
use crate::PlatformError;

/// The format every capture asks for. PipeWire converts each device's own rate and channels to it.
pub const CAPTURE_FORMAT: Format = Format { rate: 48_000, channels: 2 };

/// How long listing devices waits for the PipeWire server to answer.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(2);

/// Samples a capture's conversion buffer holds before the stream starts.
const SCRATCH_SAMPLES: usize = 8192;

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(pw::init);
}

fn not_running(e: impl std::fmt::Display) -> PlatformError {
    PlatformError::Failed(format!("could not connect to the PipeWire sound server ({e})"))
}

pub fn devices(kind: DeviceKind) -> Vec<DeviceInfo> {
    list(kind).unwrap_or_default()
}

/// The devices of `kind` the server reports, after two round trips: one for the registry's objects
/// and one for the properties of the `default` metadata object bound during the first.
fn list(kind: DeviceKind) -> Result<Vec<DeviceInfo>, PlatformError> {
    init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(not_running)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(not_running)?;
    let core = context.connect_rc(None).map_err(not_running)?;
    let registry = core.get_registry_rc().map_err(not_running)?;

    let nodes: Rc<RefCell<Vec<Node>>> = Rc::default();
    let default: Rc<RefCell<Option<String>>> = Rc::default();
    let bound: Rc<RefCell<Vec<(pw::metadata::Metadata, pw::metadata::MetadataListener)>>> = Rc::default();

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let (nodes, default, bound, registry) = (Rc::clone(&nodes), Rc::clone(&default), Rc::clone(&bound), registry.clone());
            move |g| {
                let Some(p) = g.props.as_ref() else { return };
                if g.type_ == ObjectType::Node {
                    if let Some(n) = pw_props::node(p.get(*pw::keys::MEDIA_CLASS), p.get(*pw::keys::NODE_NAME), p.get(*pw::keys::NODE_DESCRIPTION)) {
                        nodes.borrow_mut().push(n);
                    }
                } else if g.type_ == ObjectType::Metadata && p.get("metadata.name") == Some("default") {
                    let Ok(meta) = registry.bind::<pw::metadata::Metadata, _>(g) else { return };
                    let default = Rc::clone(&default);
                    let listener = meta
                        .add_listener_local()
                        .property(move |_subject, key, _type, value| {
                            if key == Some(pw_props::default_key(kind)) {
                                *default.borrow_mut() = value.and_then(pw_props::default_name);
                            }
                            0
                        })
                        .register();
                    bound.borrow_mut().push((meta, listener));
                }
            }
        })
        .register();

    // The first answer means every object has been announced; the second, every metadata property.
    let rounds = Rc::new(Cell::new(0u8));
    let waiting = Rc::new(Cell::new(core.sync(0).map_err(not_running)?));
    let _core_listener = core
        .add_listener_local()
        .done({
            let (core, mainloop, rounds, waiting, bound) =
                (core.clone(), mainloop.clone(), Rc::clone(&rounds), Rc::clone(&waiting), Rc::clone(&bound));
            move |id, seq| {
                if id != pw::core::PW_ID_CORE || seq != waiting.get() {
                    return;
                }
                rounds.set(rounds.get().saturating_add(1));
                match core.sync(0) {
                    Ok(next) if rounds.get() == 1 && !bound.borrow().is_empty() => waiting.set(next),
                    _ => mainloop.quit(),
                }
            }
        })
        .register();
    // A server that does not answer (or a connection that fails) ends the wait at the timeout.
    let timer = mainloop.loop_().add_timer({
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    let _ = timer.update_timer(Some(LIST_TIMEOUT), None);
    mainloop.run();

    let default = default.borrow().clone();
    let nodes = nodes.borrow();
    Ok(pw_props::device_list(kind, &nodes, default.as_deref()))
}

/// A capture stream; dropping it stops the stream and ends its thread.
pub struct Capture {
    /// Closing this end of the pipe wakes the capture thread's loop, which then stops.
    stop: Option<PipeWriter>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    pub fn open(
        kind: DeviceKind,
        id: &str,
        on_data: impl FnMut(&[f32], Format) + Send + 'static,
        on_error: impl FnMut(String) + Send + 'static,
    ) -> Result<(Capture, Format), PlatformError> {
        let target = if id.is_empty() || id == DEFAULT_DEVICE {
            None
        } else {
            if !list(kind)?.iter().any(|d| d.id == id) {
                return Err(PlatformError::NotFound(format!("the audio device \u{201c}{id}\u{201d}")));
            }
            Some(id.to_owned())
        };
        let (ready_tx, ready_rx) = sync_channel(1);
        let (stop_rx, stop_tx) = std::io::pipe().map_err(PlatformError::failed)?;
        let thread = std::thread::Builder::new()
            .name("audio capture".into())
            .spawn(move || {
                if let Err(e) = run(kind, target, on_data, on_error, stop_rx, &ready_tx) {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| PlatformError::Failed(e.to_string()))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok((Capture { stop: Some(stop_tx), thread: Some(thread) }, CAPTURE_FORMAT)),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err(PlatformError::Failed("the audio capture thread stopped".into()))
            }
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// What the stream's callbacks share on the capture thread.
struct StreamData<D> {
    on_data: D,
    samples: Vec<f32>,
    /// The stream has started connecting, so a return to the unconnected state is an end.
    connected: bool,
}

/// The format parameter for the stream: 32-bit float samples in [`CAPTURE_FORMAT`].
fn format_param() -> Result<Vec<u8>, PlatformError> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(CAPTURE_FORMAT.rate);
    info.set_channels(u32::from(CAPTURE_FORMAT.channels));
    let mut position = [0u32; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    for (slot, channel) in position.iter_mut().zip([spa::sys::SPA_AUDIO_CHANNEL_FL, spa::sys::SPA_AUDIO_CHANNEL_FR]) {
        *slot = channel;
    }
    info.set_position(position);
    let object = spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: info.into(),
    };
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &spa::pod::Value::Object(object))
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|e| PlatformError::Failed(format!("the audio format could not be described ({e:?})")))
}

/// Quit `mainloop` once the other end of `stop` closes, setting `stopping` first. Nothing is written
/// to the pipe; closing it makes it readable.
fn watch_stop<'l>(mainloop: &'l pw::main_loop::MainLoopRc, stop: PipeReader, stopping: Rc<Cell<bool>>) -> pw::loop_::IoSource<'l, PipeReader> {
    let quit = mainloop.clone();
    mainloop.loop_().add_io(stop, IoFlags::IN | IoFlags::HUP | IoFlags::ERR, move |_| {
        stopping.set(true);
        quit.quit();
    })
}

/// Connect a capture stream, report that it started, and run its loop until the handle is dropped.
fn run<D, E>(
    kind: DeviceKind,
    target: Option<String>,
    on_data: D,
    on_error: E,
    stop: PipeReader,
    ready: &SyncSender<Result<(), PlatformError>>,
) -> Result<(), PlatformError>
where
    D: FnMut(&[f32], Format) + Send + 'static,
    E: FnMut(String) + Send + 'static,
{
    init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(not_running)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(not_running)?;
    let core = context.connect_rc(None).map_err(not_running)?;

    // Errors reported while the capture is being stopped are not passed on.
    let stopping = Rc::new(Cell::new(false));
    let report: Rc<dyn Fn(String)> = {
        let (on_error, stopping) = (RefCell::new(on_error), Rc::clone(&stopping));
        Rc::new(move |e: String| {
            if !stopping.get()
                && let Ok(mut f) = on_error.try_borrow_mut()
            {
                f(e);
            }
        })
    };

    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Production",
        // A capture whose device goes away reports it instead of moving to another device.
        *pw::keys::NODE_DONT_RECONNECT => "true",
    };
    // The target is a name the server listed, so it holds no NUL byte.
    if let Some(t) = target {
        props.insert("target.object", t);
    }
    if kind == DeviceKind::Output {
        props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    }
    let stream = pw::stream::StreamBox::new(&core, "StudioCraft capture", props).map_err(PlatformError::failed)?;

    let data = StreamData { on_data, samples: Vec::with_capacity(SCRATCH_SAMPLES), connected: false };
    let _stream_listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed({
            let report = Rc::clone(&report);
            move |_, data, _old, new| {
                let change = match new {
                    pw::stream::StreamState::Error(e) => StreamChange::Error(e),
                    pw::stream::StreamState::Unconnected => StreamChange::Unconnected,
                    pw::stream::StreamState::Streaming => StreamChange::Streaming,
                    pw::stream::StreamState::Connecting | pw::stream::StreamState::Paused => StreamChange::Other,
                };
                if let Some(e) = pw_props::stream_failure(&change, data.connected) {
                    report(e);
                }
                data.connected = matches!(change, StreamChange::Streaming | StreamChange::Other);
            }
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            let Some(d) = buffer.datas_mut().first_mut() else { return };
            let (offset, size) = (d.chunk().offset(), d.chunk().size());
            let Some(bytes) = d.data() else { return };
            let StreamData { on_data, samples, .. } = data;
            pw_props::deliver_samples(bytes, offset, size, CAPTURE_FORMAT.channels, samples, |s| on_data(s, CAPTURE_FORMAT));
        })
        .register()
        .map_err(PlatformError::failed)?;

    // A lost server connection puts the stream in its error state, which `state_changed` reports.
    let param = format_param()?;
    let pod = spa::pod::Pod::from_bytes(&param).ok_or_else(|| PlatformError::Failed("the audio format could not be described".into()))?;
    stream
        .connect(spa::utils::Direction::Input, None, pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS, &mut [pod])
        .map_err(PlatformError::failed)?;

    let _stop = watch_stop(&mainloop, stop, stopping);
    let _ = ready.send(Ok(()));
    mainloop.run();
    Ok(())
}

/// Playback on output devices is not available on Linux yet.
pub struct Playback;

impl Playback {
    pub fn open(_id: &str, _fill: Fill, _on_error: OnError) -> Result<(Playback, Format), PlatformError> {
        Err(PlatformError::Unsupported)
    }
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

    #[test]
    fn the_format_parameter_describes_a_pod() {
        let bytes = format_param().unwrap();
        assert!(spa::pod::Pod::from_bytes(&bytes).is_some());
    }
}
