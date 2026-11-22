//! Linux: the playback streams the PipeWire server lists, and capture by linking one
//! application's playback streams to a capture stream of the app's own.
//!
//! - Listing: the server's registry announces every node and client; an application's playback
//!   stream is a node of class `Stream/Output/Audio`
//!   (https://docs.pipewire.org/page_objects_design.html, https://docs.pipewire.org/group__pw__keys.html).
//!   The registry's copy of a node's properties holds its class and owning client (`client.id`),
//!   so the backend binds each playback node to read the rest, and a client's registry properties
//!   hold the process id the server read from its socket (`pipewire.sec.pid`). Each stream is
//!   attributed to a process from these by `graph::attribute`, and each application is named by
//!   its process's program file, never by names the stream reports. A stream the sound server
//!   forwards from its PulseAudio socket carries only the process id its client wrote, so it is
//!   attributed to that process only while the process holds an open connection to that socket:
//!   the kernel's socket diagnostics give the client ends of the connections to the socket, and
//!   the process's open files in `/proc` must hold one of them. A thread of the backend's own
//!   keeps the registry and wakes the discovery when a stream starts, stops or changes state.
//! - Capture: a capture stream that the session manager does not connect, asking for 48 kHz
//!   stereo `f32`, and a link from each output port of every playback stream of the target's
//!   process tree to the capture stream's input port on the same side, made through the server's
//!   `link-factory` (https://docs.pipewire.org/page_module_link_factory.html). Links to streams
//!   that start during the capture are made as they appear. The application still plays on its
//!   own device.
//! - Meters: PipeWire does not keep a level for a stream without reading its audio, so the list
//!   has no peak readings.

mod graph;

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{PipeReader, PipeWriter, Read};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use pipewire as pw;
use pw::proxy::ProxyT;
use pw::spa;
use pw::spa::support::system::IoFlags;
use pw::types::ObjectType;

use self::graph::{Group, MAX_CLIENTS, MAX_NODES, MAX_PORTS, Port, Procs, StreamNode};
use crate::{
    AppAudioError, AppInfo, Backend, CAPTURE_WAIT_MS, CHANNELS, CaptureShared, OnData, PacketInfo, SAMPLE_RATE, Support, Target, Wake, Watcher,
};

/// The text of a failed or lost connection to the server. The window translates it through a row
/// with this text.
const NOT_CONNECTED: &str = "could not connect to the PipeWire sound server";
/// How long a lost list thread waits before it connects again.
const RECONNECT: Duration = Duration::from_secs(2);
/// How often a capture checks that its target process still runs, and how often the PulseAudio
/// sockets are read while the streams need a reading (see [`Streams::needs_reading`]).
const ALIVE_CHECK: Duration = Duration::from_secs(1);
/// How often the PulseAudio sockets are read while streams are listed and none needs a reading.
const SLOW_READINGS: Duration = Duration::from_secs(10);
/// How often the list looks for a new reading of the PulseAudio sockets.
const READING_POLL: Duration = Duration::from_millis(200);
/// Samples waiting between the stream's callback and the capture loop: 2 s of audio. Audio past
/// this is dropped.
const MAX_PENDING: usize = SAMPLE_RATE as usize * 2 * CHANNELS;
/// Samples handed to the callback at a time.
const PACKET_SAMPLES: usize = SAMPLE_RATE as usize / 100 * CHANNELS;
/// Bytes read from one `/proc/<pid>/stat` file; the kernel writes about 300.
const MAX_STAT_BYTES: u64 = 4096;
/// Runs of silent and sounding samples kept between two passes of the capture loop.
const MAX_RUNS: usize = 1024;
/// Bytes of socket diagnostics read for one listing of the system's listening Unix sockets.
const MAX_DIAG_BYTES: usize = 16 << 20;
/// Bytes received from the socket diagnostics at a time.
const DIAG_BUFFER: usize = 32 << 10;
/// How long the socket diagnostics may take to send each part of a listing.
const DIAG_WAIT: Duration = Duration::from_secs(1);
/// Open files of one process looked through for the PulseAudio socket.
const MAX_FDS: usize = 65_536;
/// Local servers read from `PULSE_SERVER`.
const MAX_PULSE_SERVERS: usize = 8;
/// Bytes read from `/etc/machine-id` or the host name file.
const MAX_NAME_FILE_BYTES: u64 = 256;
/// The program file names of PipeWire's sound servers, recognized in a process that root runs.
const SOUND_SERVERS: [&str; 2] = ["pipewire", "pipewire-pulse"];
/// `EPERM` and `EACCES`: the server refused an operation.
const REFUSED: [i32; 2] = [-1, -13];
/// `ENOENT`: a link's port went away before the server made the link.
const GONE: i32 = -2;
/// Errors on the capture's own objects kept between two passes of its loop; more are dropped.
const MAX_PROXY_ERRORS: usize = 64;
/// Times one link may fail before the capture ends.
const LINK_TRIES: u8 = 3;
/// Why a capture ends when the server could not make one of its links.
const LINK_FAILED: &str = "the application's audio could not be linked";

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(pw::init);
}

fn failed(text: &str) -> AppAudioError {
    AppAudioError::Failed(text.to_owned())
}

/// The path of the server's socket, from the environment `var` reads: `PIPEWIRE_REMOTE` as a path
/// or as a name in `PIPEWIRE_RUNTIME_DIR` or `XDG_RUNTIME_DIR`, `pipewire-0` by default.
fn socket_path(var: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let remote = var("PIPEWIRE_REMOTE").filter(|r| !r.is_empty()).unwrap_or_else(|| "pipewire-0".to_owned());
    let remote = PathBuf::from(remote);
    if remote.is_absolute() {
        return Some(remote);
    }
    let dir = var("PIPEWIRE_RUNTIME_DIR").filter(|d| !d.is_empty()).or_else(|| var("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()))?;
    Some(PathBuf::from(dir).join(remote))
}

/// The process table in `/proc`.
struct ProcFs;

/// The parent and start time of `pid`.
fn stat(pid: u32) -> Option<(u32, u64)> {
    let mut text = String::new();
    std::fs::File::open(format!("/proc/{pid}/stat")).ok()?.take(MAX_STAT_BYTES).read_to_string(&mut text).ok()?;
    graph::parse_stat(&text)
}

impl Procs for ProcFs {
    fn parent(&self, pid: u32) -> Option<u32> {
        let (parent, start) = stat(pid)?;
        if parent == 0 || parent == pid {
            return None;
        }
        let (_, parent_start) = stat(parent)?;
        (parent_start <= start).then_some(parent)
    }

    fn exe(&self, pid: u32) -> Option<String> {
        exe_at(Path::new(&format!("/proc/{pid}/exe")))
    }

    fn sound_server(&self, pid: u32) -> bool {
        sound_server_with(pid, &self.server_sockets())
    }

    fn server_sockets(&self) -> Vec<u64> {
        bound_listeners(&pulse_sockets(|k| std::env::var(k).ok(), &machine_names()))
    }

    fn started(&self, pid: u32) -> Option<u64> {
        started_at(pid)
    }

    fn pulse_connections(&self) -> Option<BTreeSet<u64>> {
        connections_to(&pulse_sockets(|k| std::env::var(k).ok(), &machine_names()))
    }

    fn holds(&self, pid: u32, inodes: &BTreeSet<u64>) -> Option<bool> {
        holds_at(Path::new("/proc"), pid, inodes)
    }
}

/// The program file the `exe` link of a process leads to. A program file removed or replaced
/// while it runs reads as `<path> (deleted)`; the suffix is kept when the file at the path that
/// ends with it is the program file itself.
fn exe_at(link: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let target = std::fs::read_link(link).ok()?;
    let text = target.to_string_lossy();
    let at_path = || match (std::fs::metadata(link), std::fs::metadata(&target)) {
        (Ok(running), Ok(file)) => running.dev() == file.dev() && running.ino() == file.ino(),
        _ => false,
    };
    match text.strip_suffix(" (deleted)") {
        Some(removed) if !at_path() => Some(removed.to_owned()),
        _ => Some(text.into_owned()),
    }
}

/// Whether `pid` is the sound server, given the inodes of the listening PulseAudio sockets in
/// `server_sockets` (see [`Procs::sound_server`]).
fn sound_server_with(pid: u32, server_sockets: &[u64]) -> bool {
    use std::os::unix::fs::MetadataExt;
    if holds_any(pid, server_sockets) {
        return true;
    }
    // A sound server that runs for the whole system belongs to root, so a user cannot read
    // its open files; a process that root runs cannot be started by the user.
    let by_root = std::fs::metadata(format!("/proc/{pid}")).is_ok_and(|m| m.uid() == 0);
    by_root && std::fs::read_link(format!("/proc/{pid}/exe")).is_ok_and(|p| p.file_name().is_some_and(|n| SOUND_SERVERS.iter().any(|s| n == *s)))
}

/// The PulseAudio sockets as one reading of the socket diagnostics found them.
#[derive(Clone, Default)]
struct Sockets {
    /// [`Procs::server_sockets`].
    servers: Vec<u64>,
    /// [`Procs::pulse_connections`].
    connections: Option<BTreeSet<u64>>,
}

/// The PulseAudio sockets, read from the socket diagnostics on a thread of their own while the
/// streams are listed (see [`Streams::reading_interval`]): at once when [`SocketScan::want`] turns
/// the readings on, then at the interval it sets. The thread ends after this is dropped.
struct SocketScan {
    /// The last reading.
    latest: Arc<ArcSwap<Reading>>,
    /// Milliseconds between readings; 0 while they are off.
    every_ms: Arc<AtomicU64>,
    /// Counts the times the readings were turned on; a reading started before the last time is
    /// not used.
    turned_on: Arc<AtomicU64>,
    /// Wakes the thread, holding at most one wake; dropping it ends the thread.
    wake: Option<std::sync::mpsc::SyncSender<()>>,
    thread: Option<JoinHandle<()>>,
}

/// One reading of a [`SocketScan`].
#[derive(Default)]
struct Reading {
    /// How many readings have ended.
    count: u64,
    /// [`SocketScan::turned_on`] when the reading started.
    turned_on: u64,
    sockets: Sockets,
}

impl SocketScan {
    /// Start the thread; `None` when the system has no thread for it.
    fn start() -> Option<SocketScan> {
        let latest = Arc::new(ArcSwap::from_pointee(Reading::default()));
        let every_ms = Arc::new(AtomicU64::new(0));
        let turned_on = Arc::new(AtomicU64::new(0));
        let (wake, woken) = std::sync::mpsc::sync_channel::<()>(1);
        let (shared, every, times) = (Arc::clone(&latest), Arc::clone(&every_ms), Arc::clone(&turned_on));
        let thread = std::thread::Builder::new()
            .name("app audio sockets".into())
            .spawn(move || {
                let mut next = Instant::now();
                loop {
                    let ms = every.load(Ordering::Acquire);
                    if ms > 0 && Instant::now() >= next {
                        let started = times.load(Ordering::Acquire);
                        let sockets = Sockets { servers: ProcFs.server_sockets(), connections: ProcFs.pulse_connections() };
                        let count = shared.load().count.wrapping_add(1);
                        shared.store(Arc::new(Reading { count, turned_on: started, sockets }));
                        next = Instant::now() + Duration::from_millis(ms);
                    }
                    let wait = if ms > 0 { next.saturating_duration_since(Instant::now()) } else { SLOW_READINGS };
                    match woken.recv_timeout(wait) {
                        // A wake asks for a reading at once.
                        Ok(()) => next = Instant::now(),
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    }
                }
            })
            .ok()?;
        Some(SocketScan { latest, every_ms, turned_on, wake: Some(wake), thread: Some(thread) })
    }

    /// Set the time between readings, or turn them off with `None`. Turning them on, or making
    /// them more frequent, starts a reading at once.
    fn want(&self, every: Option<Duration>) {
        let ms = every.map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX).max(1));
        let was = self.every_ms.load(Ordering::Acquire);
        if ms == was {
            return;
        }
        if was == 0 {
            self.turned_on.fetch_add(1, Ordering::AcqRel);
        }
        self.every_ms.store(ms, Ordering::Release);
        if ms > 0
            && (was == 0 || ms < was)
            && let Some(w) = &self.wake
        {
            let _ = w.try_send(());
        }
    }

    fn wanted(&self) -> bool {
        self.every_ms.load(Ordering::Acquire) > 0
    }

    /// How many readings have ended.
    fn readings(&self) -> u64 {
        self.latest.load().count
    }

    /// Whether the readings are on and none that started since they were turned on has ended.
    /// The streams are not attributed meanwhile.
    fn waiting(&self) -> bool {
        self.wanted() && self.sockets().is_none()
    }

    /// The last reading, while the readings are on and it started since they were turned on.
    fn sockets(&self) -> Option<Sockets> {
        if !self.wanted() {
            return None;
        }
        let since = self.turned_on.load(Ordering::Acquire);
        let l = self.latest.load();
        (l.count > 0 && l.turned_on == since).then(|| l.sockets.clone())
    }

    /// The last reading, however old; no sockets before the first.
    fn last(&self) -> Sockets {
        self.latest.load().sockets.clone()
    }
}

impl Drop for SocketScan {
    fn drop(&mut self) {
        drop(self.wake.take());
        if let Some(t) = self.thread.take() {
            crate::reaper::reap(t);
        }
    }
}

/// [`ProcFs`], with the PulseAudio sockets from a [`SocketScan`]'s last reading. The socket
/// diagnostics are read on the scan's thread only.
struct Scanned<'a>(&'a SocketScan);

impl Procs for Scanned<'_> {
    fn parent(&self, pid: u32) -> Option<u32> {
        ProcFs.parent(pid)
    }

    fn exe(&self, pid: u32) -> Option<String> {
        ProcFs.exe(pid)
    }

    fn sound_server(&self, pid: u32) -> bool {
        sound_server_with(pid, &self.server_sockets())
    }

    fn server_sockets(&self) -> Vec<u64> {
        self.0.last().servers
    }

    fn started(&self, pid: u32) -> Option<u64> {
        ProcFs.started(pid)
    }

    fn pulse_connections(&self) -> Option<BTreeSet<u64>> {
        self.0.last().connections
    }

    fn holds(&self, pid: u32, inodes: &BTreeSet<u64>) -> Option<bool> {
        ProcFs.holds(pid, inodes)
    }
}

/// Every Unix socket in one of `states` that is bound to a file, with the attributes `show` asks
/// for, from the kernel's socket diagnostics
/// (https://man7.org/linux/man-pages/man7/sock_diag.7.html). Each reply is read up to
/// [`MAX_DIAG_BYTES`] in all. The diagnostics list the sockets of this process's network
/// namespace, and need no privileges.
fn unix_sockets(states: u32, show: u32) -> Option<Vec<graph::Bound>> {
    use rustix::net::{AddressFamily, RecvFlags, SendFlags, SocketType, netlink, sockopt};
    let fd = rustix::net::socket(AddressFamily::NETLINK, SocketType::DGRAM, Some(netlink::SOCK_DIAG)).ok()?;
    sockopt::set_socket_timeout(&fd, sockopt::Timeout::Recv, Some(DIAG_WAIT)).ok()?;
    rustix::net::sendto(&fd, &graph::diag_request(states, show), SendFlags::empty(), &netlink::SocketAddrNetlink::new(0, 0)).ok()?;
    let mut found = Vec::new();
    let mut buf = vec![0u8; DIAG_BUFFER];
    let mut read = 0usize;
    while read < MAX_DIAG_BYTES {
        let (len, _) = rustix::net::recv(&fd, &mut buf[..], RecvFlags::empty()).ok()?;
        read += len.max(1);
        match graph::parse_diag(buf.get(..len)?, &mut found) {
            graph::Diag::More => {}
            graph::Diag::Done => return Some(found),
            graph::Diag::Failed => return None,
        }
    }
    None
}

/// The inodes of the listening sockets that the files at `paths` lead to now. A socket whose file
/// was removed, and a socket bound to a file that was removed and made again by another socket, is
/// not among them.
fn bound_listeners(paths: &[PathBuf]) -> Vec<u64> {
    let files = socket_files(paths);
    if files.is_empty() {
        return Vec::new();
    }
    let Some(listeners) = unix_sockets(graph::LISTEN_STATES, graph::SHOW_VFS) else { return Vec::new() };
    files.iter().filter_map(|(dev, ino)| graph::bound_to(&listeners, *dev, *ino)).collect()
}

/// The device and inode of each of the files at `paths` that is a socket now.
fn socket_files(paths: &[PathBuf]) -> Vec<(u64, u64)> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    paths.iter().filter_map(|p| std::fs::metadata(p).ok()).filter(|m| m.file_type().is_socket()).map(|m| (m.dev(), m.ino())).collect()
}

/// The inodes of the client ends of the connections to the sockets the files at `paths` lead to
/// now. The kernel binds each socket a server accepts to the file of its listening socket, and
/// reports the socket at the other end of each connection, so the client ends are the peers of
/// the connected sockets bound to one of the files. A socket bound to a file that was removed is
/// bound to another inode, and is not counted. `None` when no file at `paths` is a socket, or when
/// the system's socket table cannot be read. A client in another network namespace, such as some
/// sandboxed applications, is not counted.
fn connections_to(paths: &[PathBuf]) -> Option<BTreeSet<u64>> {
    let files = socket_files(paths);
    if files.is_empty() {
        return None;
    }
    let connected = unix_sockets(graph::CONNECTED_STATES, graph::SHOW_VFS | graph::SHOW_PEER)?;
    Some(graph::connections(&connected, &files))
}

/// A process table for the streams of the client object `client`: it asks the one it wraps
/// whether the client's process is the sound server only when `servers` has no answer for the
/// client, since the answer reads the process's open files. The PulseAudio connections are read
/// from the system's socket table once for each `connections`, when the first forwarded stream
/// needs them.
struct Remembered<'a> {
    procs: &'a dyn Procs,
    servers: &'a RefCell<BTreeMap<u32, bool>>,
    connections: &'a OnceCell<Option<BTreeSet<u64>>>,
    client: u32,
}

impl Procs for Remembered<'_> {
    fn parent(&self, pid: u32) -> Option<u32> {
        self.procs.parent(pid)
    }

    fn exe(&self, pid: u32) -> Option<String> {
        self.procs.exe(pid)
    }

    fn sound_server(&self, pid: u32) -> bool {
        if let Some(known) = self.servers.try_borrow().ok().and_then(|s| s.get(&self.client).copied()) {
            return known;
        }
        let server = self.procs.sound_server(pid);
        if let Ok(mut s) = self.servers.try_borrow_mut() {
            s.insert(self.client, server);
        }
        server
    }

    fn server_sockets(&self) -> Vec<u64> {
        self.procs.server_sockets()
    }

    fn started(&self, pid: u32) -> Option<u64> {
        self.procs.started(pid)
    }

    fn pulse_connections(&self) -> Option<BTreeSet<u64>> {
        self.connections.get_or_init(|| self.procs.pulse_connections()).clone()
    }

    fn holds(&self, pid: u32, inodes: &BTreeSet<u64>) -> Option<bool> {
        self.procs.holds(pid, inodes)
    }
}

/// The names this machine answers to in a PulseAudio server string's `{<machine>}`: its machine id,
/// its D-Bus machine id and its host name.
fn machine_names() -> Vec<String> {
    ["/etc/machine-id", "/var/lib/dbus/machine-id", "/proc/sys/kernel/hostname"]
        .iter()
        .filter_map(|f| {
            let mut text = String::new();
            std::fs::File::open(f).ok()?.take(MAX_NAME_FILE_BYTES).read_to_string(&mut text).ok()?;
            Some(text.trim().to_owned()).filter(|t| !t.is_empty())
        })
        .collect()
}

/// The paths of the user's PulseAudio sockets: the local ones `PULSE_SERVER` lists (`unix:<path>`
/// or `<path>`, each after an optional `{<machine>}` that must name this machine, one of `local`),
/// then `native` in `PULSE_RUNTIME_PATH`, or in `pulse` under `XDG_RUNTIME_DIR`
/// (https://www.freedesktop.org/wiki/Software/PulseAudio/Documentation/User/ServerStrings/).
fn pulse_sockets(var: impl Fn(&str) -> Option<String>, local: &[String]) -> Vec<PathBuf> {
    let mut sockets: Vec<PathBuf> = var("PULSE_SERVER")
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|s| {
            let s = match s.strip_prefix('{') {
                Some(rest) => {
                    let (machine, rest) = rest.split_once('}')?;
                    if !local.iter().any(|m| m == machine) {
                        return None;
                    }
                    rest
                }
                None => s,
            };
            let path = s.strip_prefix("unix:").unwrap_or(s);
            path.starts_with('/').then(|| PathBuf::from(path))
        })
        .take(MAX_PULSE_SERVERS)
        .collect();
    if let Some(dir) = var("PULSE_RUNTIME_PATH").filter(|d| !d.is_empty()) {
        sockets.push(PathBuf::from(dir).join("native"));
    } else if let Some(dir) = var("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        sockets.push(PathBuf::from(dir).join("pulse").join("native"));
    }
    sockets
}

/// Whether `pid` holds one of the sockets whose inodes are in `inodes`.
fn holds_any(pid: u32, inodes: &[u64]) -> bool {
    !inodes.is_empty() && holds_at(Path::new("/proc"), pid, &inodes.iter().copied().collect()) == Some(true)
}

/// Whether `pid` holds one of the sockets whose inodes are in `inodes`, from the links in
/// `<proc_root>/<pid>/fd`, each of which reads `socket:[<inode>]` for a socket. The system lets a
/// user read the open files of the user's own processes. `None` when the folder cannot be read:
/// the process is gone, or belongs to another user.
fn holds_at(proc_root: &Path, pid: u32, inodes: &BTreeSet<u64>) -> Option<bool> {
    let fds = std::fs::read_dir(proc_root.join(pid.to_string()).join("fd")).ok()?;
    if inodes.is_empty() {
        return Some(false);
    }
    let wanted: Vec<String> = inodes.iter().map(|inode| format!("socket:[{inode}]")).collect();
    Some(fds.take(MAX_FDS).flatten().any(|fd| std::fs::read_link(fd.path()).is_ok_and(|l| wanted.iter().any(|w| l.as_os_str() == w.as_str()))))
}

/// The start time of `pid`, which tells the process apart from a later one given the same id.
fn started_at(pid: u32) -> Option<u64> {
    stat(pid).map(|(_, start)| start)
}

/// Whether `pid` is still the process that started at `start`.
fn same_process(pid: u32, start: u64) -> bool {
    started_at(pid) == Some(start)
}

pub(crate) struct LinuxBackend {
    /// The server to connect to, for tests; `None` is the user's own.
    remote: Option<String>,
}

impl LinuxBackend {
    pub(crate) fn new() -> LinuxBackend {
        LinuxBackend { remote: None }
    }

    #[cfg(test)]
    fn with_remote(remote: &str) -> LinuxBackend {
        LinuxBackend { remote: Some(remote.to_owned()) }
    }
}

/// The connection properties for `remote`.
fn connect_props(remote: Option<&str>) -> Option<pw::properties::PropertiesBox> {
    let remote = remote?;
    let mut props = pw::properties::PropertiesBox::new();
    // The name is the backend's own, so it holds no NUL byte.
    props.insert(*pw::keys::REMOTE_NAME, remote);
    Some(props)
}

/// Whether the backend is supported with the server's socket at `socket`: when the environment
/// says where the socket is. A socket or folder that is missing, or a server that does not answer,
/// is reported in the list's error, and the list connects again every [`RECONNECT`], so a server
/// that starts later is found.
fn support_for(socket: Option<&Path>) -> Support {
    if socket.is_some() { Support::yes() } else { Support::no(crate::NOT_AVAILABLE) }
}

/// Whether a connection to the server at `socket` can be tried: a socket file is there. Clients
/// reach the server through a Unix socket (https://docs.pipewire.org/page_module_protocol_native.html).
/// Without a socket, the list and the capture fail with [`NOT_CONNECTED`] before they make a
/// PipeWire context, which logs each configuration file it cannot read; the list tries again
/// every [`RECONNECT`].
fn can_connect(socket: Option<&Path>) -> bool {
    use std::os::unix::fs::FileTypeExt;
    socket.and_then(|s| std::fs::metadata(s).ok()).is_some_and(|m| m.file_type().is_socket())
}

impl LinuxBackend {
    /// The server's socket: the test server's, or the user's own from the environment.
    fn socket(&self) -> Option<PathBuf> {
        server_socket(self.remote.as_deref())
    }
}

/// The socket of the server at `remote`, or of the user's own server when it is `None`.
fn server_socket(remote: Option<&str>) -> Option<PathBuf> {
    match remote {
        Some(r) => Some(PathBuf::from(r)),
        None => socket_path(|k| std::env::var(k).ok()),
    }
}

impl Backend for LinuxBackend {
    fn support(&self) -> Support {
        support_for(self.socket().as_deref())
    }

    fn watch(&self, wake: Wake) -> Result<Box<dyn Watcher>, AppAudioError> {
        let mut w =
            LinuxWatcher { remote: self.remote.clone(), wake, list: None, next_start: Instant::now(), chosen: BTreeMap::new(), cleared: false };
        w.start();
        Ok(Box::new(w))
    }

    fn capture(&self, target: Target, shared: &CaptureShared, on_data: &mut OnData) -> Result<(), AppAudioError> {
        shared.beat();
        let Some(start) = started_at(target.pid) else {
            return Err(AppAudioError::NotRunning(format!("process {}", target.pid)));
        };
        if !can_connect(self.socket().as_deref()) {
            return Err(failed(NOT_CONNECTED));
        }
        init();
        let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|_| failed(NOT_CONNECTED))?;
        let context = pw::context::ContextRc::new(&mainloop, None).map_err(|_| failed(NOT_CONNECTED))?;
        let core = context.connect_rc(connect_props(self.remote.as_deref())).map_err(|_| failed(NOT_CONNECTED))?;
        shared.beat();
        run_capture(&mainloop, &core, (target, start), shared, on_data)
    }
}

/// What the list thread shares with the watcher.
#[derive(Default)]
struct Listed {
    nodes: BTreeMap<u32, StreamNode>,
    /// Why the thread stopped, once it has.
    error: Option<String>,
}

/// The list thread and the way to stop it.
struct ListThread {
    listed: Arc<Mutex<Listed>>,
    /// Closing this end of the pipe wakes the thread's loop, which then stops.
    stop: Option<PipeWriter>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for ListThread {
    fn drop(&mut self) {
        drop(self.stop.take());
        // The loop ends within one wake of the pipe; the reaper joins it, so the discovery thread
        // never waits on the server.
        if let Some(t) = self.thread.take() {
            crate::reaper::reap(t);
        }
    }
}

struct LinuxWatcher {
    remote: Option<String>,
    wake: Wake,
    list: Option<ListThread>,
    /// The soonest a lost list thread is started again.
    next_start: Instant,
    /// The process tree each application's capture took in the last scan.
    chosen: BTreeMap<Group, u32>,
    /// A scan since the list thread failed has listed no applications.
    cleared: bool,
}

impl LinuxWatcher {
    fn start(&mut self) {
        self.next_start = Instant::now() + RECONNECT;
        let listed = Arc::new(Mutex::new(Listed::default()));
        let Ok((reader, writer)) = std::io::pipe() else {
            if let Ok(mut l) = listed.lock() {
                l.error = Some(NOT_CONNECTED.to_owned());
            }
            self.list = Some(ListThread { listed, stop: None, thread: None });
            return;
        };
        let (shared, wake, remote) = (Arc::clone(&listed), self.wake.clone(), self.remote.clone());
        let thread = std::thread::Builder::new()
            .name("app audio list".into())
            .spawn(move || {
                if let Err(e) = list_loop(remote.as_deref(), &shared, &wake, reader)
                    && let Ok(mut l) = shared.lock()
                {
                    l.nodes.clear();
                    l.error = Some(e.to_owned());
                }
                wake.wake();
            })
            .ok();
        if thread.is_none()
            && let Ok(mut l) = listed.lock()
        {
            l.error = Some(NOT_CONNECTED.to_owned());
        }
        self.list = Some(ListThread { listed, stop: Some(writer), thread });
    }

    /// The streams the list thread holds, or why it stopped. A thread that stopped is started
    /// again at most every [`RECONNECT`].
    fn streams(&mut self) -> Result<BTreeMap<u32, StreamNode>, AppAudioError> {
        let ended = self.list.as_ref().is_none_or(|l| l.thread.as_ref().is_none_or(JoinHandle::is_finished));
        let current = self.list.as_ref().and_then(|l| l.listed.lock().ok().map(|g| (g.nodes.clone(), g.error.clone())));
        if ended && Instant::now() >= self.next_start {
            self.start();
        }
        let error = match current {
            Some((nodes, None)) => {
                self.cleared = false;
                return Ok(nodes);
            }
            Some((_, Some(e))) => e,
            None => NOT_CONNECTED.to_owned(),
        };
        // The discovery keeps its last list when a scan fails, so the first scan after the server
        // goes away lists no applications, and the next one, soon after, reports why.
        if !self.cleared {
            self.cleared = true;
            self.wake.wake();
            return Ok(BTreeMap::new());
        }
        Err(AppAudioError::List(error))
    }
}

impl Watcher for LinuxWatcher {
    fn scan(&mut self) -> Result<Vec<AppInfo>, AppAudioError> {
        let nodes = self.streams()?;
        let (apps, chosen) = graph::apps(&nodes, &ProcFs, &self.chosen);
        self.chosen = chosen;
        Ok(apps)
    }

    fn peaks(&mut self, _apps: &[AppInfo], _out: &mut dyn FnMut(usize, f32)) {}
}

/// Quit `mainloop` once the other end of `stop` closes. Nothing is written to the pipe; closing it
/// makes it readable.
fn watch_stop<'l>(mainloop: &'l pw::main_loop::MainLoopRc, stop: PipeReader) -> pw::loop_::IoSource<'l, PipeReader> {
    let quit = mainloop.clone();
    mainloop.loop_().add_io(stop, IoFlags::IN | IoFlags::HUP | IoFlags::ERR, move |_| quit.quit())
}

/// What the backend knows of a playback stream node it follows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Followed {
    /// The client object that owns the node, from the registry's copy of its properties.
    client: Option<u32>,
    /// The process the node's properties claim (`application.process.id`), once they arrived.
    claimed: Option<u32>,
    running: bool,
}

/// The proxy that reports a followed node's state and properties. The listener is declared first
/// so it is dropped, and its hook removed, before the proxy.
struct NodeProxy {
    _listener: pw::node::NodeListener,
    _proxy: pw::node::Node,
}

/// The playback streams the server lists and the process id it read for each client.
#[derive(Default)]
struct Streams {
    nodes: BTreeMap<u32, Followed>,
    proxies: BTreeMap<u32, NodeProxy>,
    /// Each client object's `pipewire.sec.pid`.
    clients: BTreeMap<u32, u32>,
    /// Whether each client object's process is the sound server, kept while the client is listed
    /// and the sockets in `sockets` stay the same, since the answer reads the process's open
    /// files. A client's process does not change; a process id the system reuses comes with a new
    /// client.
    servers: RefCell<BTreeMap<u32, bool>>,
    /// [`Procs::server_sockets`] when `servers` was last checked.
    sockets: RefCell<Vec<u64>>,
}

impl Streams {
    /// The followed streams attributed to their processes; a stream attributed to none is left
    /// out. Whether the process a forwarded stream claims holds a PulseAudio connection is checked
    /// again on each call, which the list thread makes whenever a followed stream, or the client
    /// that owns one, appears, goes or changes, and after each new reading of a [`SocketScan`].
    fn attributed(&self, procs: &dyn Procs) -> BTreeMap<u32, StreamNode> {
        if let (Ok(mut servers), Ok(mut sockets)) = (self.servers.try_borrow_mut(), self.sockets.try_borrow_mut()) {
            let now = procs.server_sockets();
            if *sockets != now {
                servers.clear();
                *sockets = now;
            }
            servers.retain(|client, _| self.clients.contains_key(client));
        }
        let connections = OnceCell::new();
        self.nodes
            .iter()
            .filter_map(|(id, f)| {
                let client = f.client?;
                let real = self.clients.get(&client).copied();
                let procs = Remembered { procs, servers: &self.servers, connections: &connections, client };
                graph::attribute(f.claimed, real, &procs).map(|pid| (*id, StreamNode { pid, running: f.running }))
            })
            .collect()
    }

    /// Whether a followed stream claims a process other than the one the server read for its
    /// client. The attribution of such a stream depends on the claimed process's PulseAudio
    /// connection, which the registry does not report.
    fn claims_another(&self) -> bool {
        self.nodes.values().any(|f| {
            let real = f.client.and_then(|c| self.clients.get(&c));
            f.claimed.is_some_and(|claimed| real.is_some_and(|r| *r != claimed))
        })
    }

    /// Whether attributing the streams needs a reading of the PulseAudio sockets: a stream claims
    /// another process, or the listed client of a stream is not yet known to be, or not to be,
    /// the sound server.
    fn needs_reading(&self) -> bool {
        self.claims_another()
            || self
                .servers
                .try_borrow()
                .is_ok_and(|known| self.nodes.values().filter_map(|f| f.client).any(|c| self.clients.contains_key(&c) && !known.contains_key(&c)))
    }

    /// The time between readings of the PulseAudio sockets: [`ALIVE_CHECK`] while the streams
    /// need a reading, [`SLOW_READINGS`] while other streams are followed, and none without
    /// streams.
    fn reading_interval(&self) -> Option<Duration> {
        if self.needs_reading() {
            Some(ALIVE_CHECK)
        } else if self.nodes.is_empty() {
            None
        } else {
            Some(SLOW_READINGS)
        }
    }

    /// Forget the object `id`; returns whether it was a followed stream or a client one owns.
    fn remove(&mut self, id: u32) -> bool {
        self.proxies.remove(&id);
        let node = self.nodes.remove(&id).is_some();
        let pid = self.clients.remove(&id);
        if let Ok(mut servers) = self.servers.try_borrow_mut() {
            servers.remove(&id);
        }
        let client = pid.is_some() && self.nodes.values().any(|f| f.client == Some(id));
        node || client
    }
}

/// Follow a client or a playback node that the registry announced, and call `changed` when what
/// the streams are attributed to may have changed.
fn follow(
    streams: &Rc<RefCell<Streams>>,
    registry: &pw::registry::RegistryRc,
    g: &pw::registry::GlobalObject<&spa::utils::dict::DictRef>,
    changed: &Rc<dyn Fn()>,
) {
    let Some(p) = g.props.as_ref() else { return };
    match g.type_ {
        ObjectType::Client => {
            let Some(pid) = graph::parse_id(p.get(*pw::keys::SEC_PID)) else { return };
            let owns = {
                let Ok(mut s) = streams.try_borrow_mut() else { return };
                if s.clients.len() >= MAX_CLIENTS {
                    return;
                }
                s.clients.insert(g.id, pid);
                s.nodes.values().any(|f| f.client == Some(g.id))
            };
            if owns {
                changed();
            }
        }
        ObjectType::Node if graph::is_playback(p.get(*pw::keys::MEDIA_CLASS)) => {
            if streams.try_borrow().map_or(true, |s| s.nodes.len() >= MAX_NODES) {
                return;
            }
            let Ok(proxy) = registry.bind::<pw::node::Node, _>(g) else { return };
            let id = g.id;
            let listener = proxy
                .add_listener_local()
                .info({
                    let (streams, changed) = (Rc::clone(streams), Rc::clone(changed));
                    move |info| {
                        let running = matches!(info.state(), pw::node::NodeState::Running);
                        let claimed = info
                            .change_mask()
                            .contains(pw::node::NodeChangeMask::PROPS)
                            .then(|| info.props().and_then(|p| graph::parse_id(p.get(*pw::keys::APP_PROCESS_ID))));
                        let did = streams.try_borrow_mut().ok().and_then(|mut s| {
                            let f = s.nodes.get_mut(&id)?;
                            let before = (f.running, f.claimed);
                            f.running = running;
                            if let Some(c) = claimed {
                                f.claimed = c;
                            }
                            Some(before != (f.running, f.claimed))
                        });
                        if did == Some(true) {
                            changed();
                        }
                    }
                })
                .register();
            let client = graph::parse_id(p.get(*pw::keys::CLIENT_ID));
            let Ok(mut s) = streams.try_borrow_mut() else { return };
            s.nodes.insert(id, Followed { client, claimed: None, running: false });
            s.proxies.insert(id, NodeProxy { _proxy: proxy, _listener: listener });
            drop(s);
            // A client that claims nothing is attributed before its node's properties arrive.
            changed();
        }
        _ => {}
    }
}

/// Keep `listed` current with the server's playback streams until `stop` closes or the server
/// goes away.
fn list_loop(remote: Option<&str>, listed: &Arc<Mutex<Listed>>, wake: &Wake, stop: PipeReader) -> Result<(), &'static str> {
    if !can_connect(server_socket(remote).as_deref()) {
        return Err(NOT_CONNECTED);
    }
    init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|_| NOT_CONNECTED)?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|_| NOT_CONNECTED)?;
    let core = context.connect_rc(connect_props(remote)).map_err(|_| NOT_CONNECTED)?;
    let registry = core.get_registry_rc().map_err(|_| NOT_CONNECTED)?;

    let streams: Rc<RefCell<Streams>> = Rc::default();
    let scan = Rc::new(SocketScan::start().ok_or(NOT_CONNECTED)?);
    let publish: Rc<dyn Fn()> = {
        let (streams, scan, listed, wake) = (Rc::clone(&streams), Rc::clone(&scan), Arc::clone(listed), wake.clone());
        Rc::new(move || {
            let Ok(s) = streams.try_borrow() else { return };
            scan.want(s.reading_interval());
            if scan.waiting() {
                return;
            }
            let nodes = s.attributed(&Scanned(&scan));
            drop(s);
            if let Ok(mut l) = listed.lock() {
                l.nodes = nodes;
            }
            wake.wake();
        })
    };

    let _registry_listener = registry
        .add_listener_local()
        .global({
            let (streams, publish, registry) = (Rc::clone(&streams), Rc::clone(&publish), registry.clone());
            move |g| follow(&streams, &registry, g, &publish)
        })
        .global_remove({
            let (streams, publish) = (Rc::clone(&streams), Rc::clone(&publish));
            move |id| {
                let removed = streams.try_borrow_mut().is_ok_and(|mut s| s.remove(id));
                if removed {
                    publish();
                }
            }
        })
        .register();

    // A lost connection reports an error on the core object and ends the loop.
    let lost = Rc::new(Cell::new(false));
    let _core_listener = core
        .add_listener_local()
        .error({
            let (mainloop, lost) = (mainloop.clone(), Rc::clone(&lost));
            move |id, _seq, _res, _message| {
                if id == pw::core::PW_ID_CORE {
                    lost.set(true);
                    mainloop.quit();
                }
            }
        })
        .register();
    // A process can close its PulseAudio connection with no change in the registry, so each new
    // reading of the sockets attributes the streams again.
    let seen = Cell::new(0);
    let recheck = mainloop.loop_().add_timer({
        let (streams, scan, listed, wake) = (Rc::clone(&streams), Rc::clone(&scan), Arc::clone(listed), wake.clone());
        move |_| {
            let Ok(interval) = streams.try_borrow().map(|s| s.reading_interval()) else { return };
            scan.want(interval);
            let readings = scan.readings();
            if interval.is_some() && !scan.waiting() && readings != seen.replace(readings) && republish(&streams, &Scanned(&scan), &listed) {
                wake.wake();
            }
        }
    });
    let _ = recheck.update_timer(Some(READING_POLL), Some(READING_POLL));
    let _stop = watch_stop(&mainloop, stop);
    mainloop.run();
    if lost.get() { Err(NOT_CONNECTED) } else { Ok(()) }
}

/// Attribute the streams in `streams` with `procs` again, and put them in `listed` when they
/// differ from the ones it holds. Returns whether they differed.
fn republish(streams: &RefCell<Streams>, procs: &dyn Procs, listed: &Mutex<Listed>) -> bool {
    let Ok(s) = streams.try_borrow() else { return false };
    let nodes = s.attributed(procs);
    drop(s);
    let Ok(mut l) = listed.lock() else { return false };
    if l.nodes == nodes {
        return false;
    }
    l.nodes = nodes;
    true
}

/// The format parameter for the capture stream: 32-bit float samples, 48 kHz, left and right.
fn format_param() -> Result<Vec<u8>, AppAudioError> {
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(SAMPLE_RATE);
    info.set_channels(CHANNELS as u32);
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
        .map_err(|_| failed("the audio format could not be described"))
}

/// `SPA_CHUNK_FLAG_EMPTY`: the chunk holds silence, whatever its bytes are
/// (https://docs.pipewire.org/structspa__chunk.html).
const CHUNK_EMPTY: i32 = 1 << 1;

/// Append the whole frames of a buffer's valid region to `pending`, as samples, or silence for
/// them when the chunk is `empty`. Values that are not finite become silence, and samples past
/// [`MAX_PENDING`] are dropped. Returns how many samples were dropped.
fn take_samples(bytes: &[u8], offset: u32, size: u32, empty: bool, pending: &mut Vec<f32>) -> usize {
    let len = bytes.len();
    if empty {
        // The caller clamps `size` to the buffer's size; an empty chunk may come with no data.
        let samples = usize::try_from(size).unwrap_or(usize::MAX) / (4 * CHANNELS) * CHANNELS;
        let room = MAX_PENDING.saturating_sub(pending.len());
        pending.resize(pending.len() + room.min(samples), 0.0);
        return samples.saturating_sub(room);
    }
    if len == 0 {
        return 0;
    }
    // PipeWire's chunk offset is taken modulo the buffer's size, and its size is clamped to the
    // buffer's size (https://docs.pipewire.org/structspa__chunk.html), so a region that runs past
    // the end goes on from the start.
    let start = usize::try_from(offset).map_or(0, |o| o % len);
    let size = usize::try_from(size).unwrap_or(usize::MAX).min(len);
    let frame = 4 * CHANNELS;
    let whole = size / frame * frame;
    let (Some(tail), Some(head)) = (bytes.get(start..), bytes.get(..start)) else { return 0 };
    let mut region = tail.iter().chain(head).take(whole).copied();
    let room = MAX_PENDING.saturating_sub(pending.len());
    let samples = whole / 4;
    for _ in 0..room.min(samples) {
        let (Some(a), Some(b), Some(c), Some(d)) = (region.next(), region.next(), region.next(), region.next()) else { break };
        let s = f32::from_le_bytes([a, b, c, d]);
        pending.push(if s.is_finite() { s } else { 0.0 });
    }
    samples.saturating_sub(room)
}

/// Samples waiting for the capture loop, and which of them came from chunks the server marked as
/// silence.
#[derive(Default)]
struct Pending {
    samples: Vec<f32>,
    /// Runs of `samples` in order, as `(end, silent)`; at most [`MAX_RUNS`], past which a run
    /// takes in the next chunks and counts as silent only if all of them are.
    runs: Vec<(usize, bool)>,
}

impl Pending {
    /// A buffer with room for [`MAX_PENDING`] samples and [`MAX_RUNS`] runs, so the stream's
    /// callback never allocates.
    fn allocated() -> Pending {
        Pending { samples: Vec::with_capacity(MAX_PENDING), runs: Vec::with_capacity(MAX_RUNS) }
    }

    /// Append a chunk's samples (see [`take_samples`]); returns how many samples were dropped.
    fn take(&mut self, bytes: &[u8], offset: u32, size: u32, empty: bool) -> usize {
        let dropped = take_samples(bytes, offset, size, empty, &mut self.samples);
        let (end, start, full) = (self.samples.len(), self.runs.last().map_or(0, |r| r.0), self.runs.len() >= MAX_RUNS);
        if end > start {
            match self.runs.last_mut() {
                Some(last) if last.1 == empty || full => *last = (end, last.1 && empty),
                _ => self.runs.push((end, empty)),
            }
        }
        dropped
    }

    fn clear(&mut self) {
        self.samples.clear();
        self.runs.clear();
    }
}

/// Whether every sample in `from..to` lies in a silent run of `runs` (see [`Pending::runs`]).
fn all_silent(runs: &[(usize, bool)], from: usize, to: usize) -> bool {
    let mut start = 0;
    let mut covered = from;
    for &(end, silent) in runs {
        if end > from && start < to {
            if !silent {
                return false;
            }
            covered = covered.max(end.min(to));
        }
        start = end;
    }
    from < to && covered >= to
}

/// Where a capture's next packet sits in its stream.
#[derive(Default)]
struct Clock {
    /// The stream position of the next packet's first frame, in frames from the start of the
    /// capture.
    position: u64,
    /// Audio was dropped before the next packet.
    gap: bool,
}

impl Clock {
    /// Hand `pending`'s samples to `out` in packets of [`PACKET_SAMPLES`], each with its position
    /// and marked silent when all of it came from chunks the server marked as silence, then count
    /// `lost` samples that were dropped after them: the next packet starts after the gap and is
    /// marked as following lost audio. PipeWire gives the capture no performance counter time.
    fn deliver(&mut self, pending: &Pending, lost: u64, mut out: impl FnMut(&[f32], PacketInfo)) {
        for (i, packet) in pending.samples.chunks(PACKET_SAMPLES).enumerate() {
            let from = i * PACKET_SAMPLES;
            let silent = all_silent(&pending.runs, from, from + packet.len());
            let info = PacketInfo { position: self.position, qpc_100ns: None, discontinuity: std::mem::take(&mut self.gap), silent };
            out(packet, info);
            self.position = self.position.saturating_add((packet.len() / CHANNELS) as u64);
        }
        if lost > 0 {
            self.position = self.position.saturating_add(lost / CHANNELS as u64);
            self.gap = true;
        }
    }
}

/// What the capture's callbacks tell the capture loop.
#[derive(Default)]
struct Events {
    /// The stream connected (paused or streaming).
    connected: bool,
    /// Why the capture must end.
    error: Option<AppAudioError>,
    /// The graph changed since the links were last planned.
    dirty: bool,
    /// Errors the server reported on the capture's own objects, as `(proxy id, result)`.
    proxy_errors: Vec<(u32, i32)>,
    /// Links that entered the error state after the server made them.
    failed_links: Vec<LinkKey>,
}

/// The playback streams and ports a capture follows.
#[derive(Default)]
struct Graph {
    streams: Rc<RefCell<Streams>>,
    ports: BTreeMap<u32, Port>,
}

/// Run one capture on the calling thread until `shared` asks it to stop. `start` is the target's
/// start time, so the capture ends when the target exits even if a new process gets its id.
fn run_capture(
    mainloop: &pw::main_loop::MainLoopRc,
    core: &pw::core::CoreRc,
    (target, start): (Target, u64),
    shared: &CaptureShared,
    on_data: &mut OnData,
) -> Result<(), AppAudioError> {
    let registry = core.get_registry_rc().map_err(|_| failed(NOT_CONNECTED))?;
    let events: Rc<RefCell<Events>> = Rc::default();
    let graph: Rc<RefCell<Graph>> = Rc::default();
    let pending: Rc<RefCell<Pending>> = Rc::new(RefCell::new(Pending::allocated()));
    // Samples dropped since the capture loop last took `pending`; they all come after its samples.
    let dropped: Rc<Cell<u64>> = Rc::default();
    let scan = SocketScan::start().ok_or_else(|| failed(NOT_CONNECTED))?;

    let mark_dirty: Rc<dyn Fn()> = {
        let events = Rc::clone(&events);
        Rc::new(move || {
            if let Ok(mut e) = events.try_borrow_mut() {
                e.dirty = true;
            }
        })
    };
    let _registry_listener = registry
        .add_listener_local()
        .global({
            let (graph, mark_dirty, registry) = (Rc::clone(&graph), Rc::clone(&mark_dirty), registry.clone());
            move |g| {
                let Ok(mut gr) = graph.try_borrow_mut() else { return };
                if g.type_ != ObjectType::Port {
                    let streams = Rc::clone(&gr.streams);
                    drop(gr);
                    follow(&streams, &registry, g, &mark_dirty);
                    return;
                }
                let Some(p) = g.props.as_ref() else { return };
                if gr.ports.len() >= MAX_PORTS {
                    return;
                }
                let Some(port) = graph::port(
                    p.get(*pw::keys::PORT_DIRECTION),
                    p.get(*pw::keys::NODE_ID),
                    p.get(*pw::keys::AUDIO_CHANNEL),
                    p.get(*pw::keys::PORT_MONITOR),
                ) else {
                    return;
                };
                gr.ports.insert(g.id, port);
                drop(gr);
                mark_dirty();
            }
        })
        .global_remove({
            let (graph, mark_dirty) = (Rc::clone(&graph), Rc::clone(&mark_dirty));
            move |id| {
                let Ok(mut gr) = graph.try_borrow_mut() else { return };
                let removed = gr.streams.try_borrow_mut().is_ok_and(|mut s| s.remove(id)) | gr.ports.remove(&id).is_some();
                drop(gr);
                if removed {
                    mark_dirty();
                }
            }
        })
        .register();

    let _core_listener = core
        .add_listener_local()
        .error({
            let events = Rc::clone(&events);
            move |id, _seq, res, _message| {
                let Ok(mut e) = events.try_borrow_mut() else { return };
                if id == pw::core::PW_ID_CORE {
                    // An ENOENT on the core answers the removal of a link the server had already
                    // removed (see `unlink`).
                    if res != GONE {
                        e.error.get_or_insert_with(|| failed(NOT_CONNECTED));
                    }
                } else if e.proxy_errors.len() < MAX_PROXY_ERRORS {
                    // The capture loop matches the id with its links.
                    e.proxy_errors.push((id, res));
                }
            }
        })
        .register();

    let props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Production",
        *pw::keys::NODE_DONT_RECONNECT => "true",
    };
    let stream =
        pw::stream::StreamBox::new(core, "StudioCraft application capture", props).map_err(|_| failed("the audio capture could not start"))?;
    let _stream_listener = stream
        .add_local_listener_with_user_data(())
        .state_changed({
            let events = Rc::clone(&events);
            move |_, _, _old, new| {
                let Ok(mut e) = events.try_borrow_mut() else { return };
                match new {
                    // The server's own text is left out: the window translates fixed messages only.
                    pw::stream::StreamState::Error(_) => {
                        e.error.get_or_insert_with(|| failed("the audio stream failed"));
                    }
                    pw::stream::StreamState::Unconnected if e.connected => {
                        e.error.get_or_insert_with(|| failed("the audio stream ended"));
                    }
                    pw::stream::StreamState::Paused | pw::stream::StreamState::Streaming => {
                        e.connected = true;
                        e.dirty = true;
                    }
                    pw::stream::StreamState::Unconnected | pw::stream::StreamState::Connecting => {}
                }
            }
        })
        .process({
            let (pending, dropped) = (Rc::clone(&pending), Rc::clone(&dropped));
            move |stream, _| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let Some(d) = buffer.datas_mut().first_mut() else { return };
                let (offset, size) = (d.chunk().offset(), d.chunk().size().min(d.as_raw().maxsize));
                let empty = d.chunk().flags().bits() & CHUNK_EMPTY != 0;
                let bytes: &[u8] = match d.data() {
                    Some(b) => b,
                    None if empty => &[],
                    None => return,
                };
                if let Ok(mut p) = pending.try_borrow_mut() {
                    let lost = p.take(bytes, offset, size, empty);
                    dropped.set(dropped.get().saturating_add(lost as u64));
                }
            }
        })
        .register()
        .map_err(|_| failed("the audio capture could not start"))?;

    let param = format_param()?;
    let pod = spa::pod::Pod::from_bytes(&param).ok_or_else(|| failed("the audio format could not be described"))?;
    // Without AUTOCONNECT the session manager links nothing to the stream; the capture makes
    // every link itself.
    stream
        .connect(spa::utils::Direction::Input, None, pw::stream::StreamFlags::MAP_BUFFERS, &mut [pod])
        .map_err(|_| failed("the audio capture could not start"))?;

    let mut links: BTreeMap<LinkKey, CaptureLink> = BTreeMap::new();
    let mut link_failures: BTreeMap<LinkKey, u8> = BTreeMap::new();
    let mut out = Pending::allocated();
    let (mut started, mut flowing) = (false, false);
    let mut clock = Clock::default();
    let mut next_alive = Instant::now() + ALIVE_CHECK;
    let mut seen_reading = 0;
    let wait = Duration::from_millis(u64::from(CAPTURE_WAIT_MS));
    let result = loop {
        if shared.stopping() {
            break Ok(());
        }
        shared.beat();
        mainloop.loop_().iterate(pw::loop_::Timeout::Finite(wait));
        let (error, connected, mut dirty, proxy_errors, failed_links) = match events.try_borrow_mut() {
            Ok(mut e) => {
                (e.error.take(), e.connected, std::mem::take(&mut e.dirty), std::mem::take(&mut e.proxy_errors), std::mem::take(&mut e.failed_links))
            }
            Err(_) => (None, false, false, Vec::new(), Vec::new()),
        };
        if let Some(e) = error {
            break Err(e);
        }
        // Errors with a result code come first, so a refusal is reported as one; a link whose
        // state alone says it failed is made again, up to `LINK_TRIES` times.
        if !proxy_errors.is_empty() {
            let ids: BTreeMap<u32, LinkKey> = links.iter().map(|(k, l)| (l.link.upcast_ref().id(), *k)).collect();
            let (forget, end) = link_errors(&proxy_errors, &ids);
            if let Some(e) = end {
                break Err(e);
            }
            for k in forget {
                if let Some(l) = links.remove(&k) {
                    unlink(core, l);
                }
                dirty = true;
            }
        }
        let mut gave_up = false;
        for k in failed_links {
            if let Some(l) = links.remove(&k) {
                unlink(core, l);
                dirty = true;
                gave_up |= link_failed(&mut link_failures, k);
            }
        }
        if gave_up {
            break Err(failed(LINK_FAILED));
        }
        // The links follow the streams' attribution, which is made again after each new reading
        // of the PulseAudio sockets.
        if let Some(interval) = graph.try_borrow().ok().and_then(|g| g.streams.try_borrow().ok().map(|s| s.reading_interval())) {
            scan.want(interval);
        }
        let readings = scan.readings();
        dirty |= readings != std::mem::replace(&mut seen_reading, readings);
        let ours = stream.node_id();
        if dirty
            && connected
            && ours != spa::sys::SPA_ID_INVALID
            && !scan.waiting()
            && let Err(e) = relink(core, &graph, &events, &Scanned(&scan), (&mut links, &mut link_failures), (target.pid, ours))
        {
            break Err(e);
        }
        if connected && !started {
            started = true;
            shared.started();
        }
        // The samples are moved out first, so the callback runs without the stream's buffer held.
        let mut lost = 0;
        if let Ok(mut p) = pending.try_borrow_mut() {
            std::mem::swap(&mut *p, &mut out);
            lost = dropped.replace(0);
        }
        clock.deliver(&out, lost, |packet, info| {
            on_data(packet, info);
            shared.packet(&info);
            if !flowing {
                flowing = true;
                shared.running();
            }
        });
        out.clear();
        if Instant::now() >= next_alive {
            next_alive = Instant::now() + ALIVE_CHECK;
            if !same_process(target.pid, start) {
                break Err(AppAudioError::NotRunning(format!("process {}", target.pid)));
            }
        }
    };
    for (_, link) in std::mem::take(&mut links) {
        unlink(core, link);
    }
    let _ = stream.disconnect();
    result
}

/// One link of a capture: `(output port, output node, input port)`.
type LinkKey = (u32, u32, u32);

/// What the server's errors on a capture's own objects mean, given the proxy id of each of its
/// links: the links to forget because their port went away first, and the error that ends the
/// capture. A refusal ends it as [`AppAudioError::Denied`], any other failure of a link as
/// [`AppAudioError::Failed`]. Errors on objects that are not links are left to the stream's own
/// state.
fn link_errors(errors: &[(u32, i32)], links: &BTreeMap<u32, LinkKey>) -> (Vec<LinkKey>, Option<AppAudioError>) {
    let mut forget = Vec::new();
    for (id, res) in errors {
        let Some(key) = links.get(id) else { continue };
        if *res == GONE {
            forget.push(*key);
        } else if REFUSED.contains(res) {
            return (forget, Some(AppAudioError::Denied("the system refused to link the application's audio".into())));
        } else {
            return (forget, Some(failed(LINK_FAILED)));
        }
    }
    (forget, None)
}

/// Count a failure of the link `key` in `failures`; true when it has failed [`LINK_TRIES`] times,
/// as a link between ports that agree on no format does each time it is made. [`relink`] forgets
/// the counts of links the graph no longer needs, so `failures` holds at most `graph::MAX_LINKS`.
fn link_failed(failures: &mut BTreeMap<LinkKey, u8>, key: LinkKey) -> bool {
    let n = failures.entry(key).or_insert(0);
    *n = n.saturating_add(1);
    *n >= LINK_TRIES
}

/// Forget the failure counts of links that are not in `wanted`.
fn forget_failures(failures: &mut BTreeMap<LinkKey, u8>, wanted: &BTreeSet<LinkKey>) {
    failures.retain(|k, _| wanted.contains(k));
}

/// One link a capture made, with the listener that reports when the link fails. The
/// listener is declared first so it is dropped, and its hook removed, before the proxy.
struct CaptureLink {
    _listener: pw::link::LinkListener,
    link: pw::link::Link,
}

/// Remove a link from the server, not only the capture's proxy for it. A link the server already
/// removed draws an `ENOENT` error on the core, which the capture ignores.
fn unlink(core: &pw::core::CoreRc, link: CaptureLink) {
    let CaptureLink { _listener, link } = link;
    drop(_listener);
    let _ = core.destroy_object(link);
}

/// Make the links the graph now needs and remove the ones it no longer does, for the capture of
/// `target` into the node `ours`. A link the server accepts but then fails, for example when its
/// ports agree on no format or a port went away, is reported in [`Events::failed_links`].
fn relink(
    core: &pw::core::CoreRc,
    graph: &Rc<RefCell<Graph>>,
    events: &Rc<RefCell<Events>>,
    procs: &dyn Procs,
    (links, failures): (&mut BTreeMap<LinkKey, CaptureLink>, &mut BTreeMap<LinkKey, u8>),
    (target, ours): (u32, u32),
) -> Result<(), AppAudioError> {
    let Ok(g) = graph.try_borrow() else { return Ok(()) };
    let Ok(nodes) = g.streams.try_borrow().map(|s| s.attributed(procs)) else { return Ok(()) };
    let wanted = graph::plan(target, &nodes, &g.ports, ours, procs);
    forget_failures(failures, &wanted);
    let obsolete: Vec<LinkKey> = links.keys().filter(|k| !wanted.contains(k)).copied().collect();
    for k in obsolete {
        if let Some(l) = links.remove(&k) {
            unlink(core, l);
        }
    }
    for (out_port, out_node, in_port) in wanted {
        if links.contains_key(&(out_port, out_node, in_port)) {
            continue;
        }
        let props = pw::properties::properties! {
            *pw::keys::LINK_OUTPUT_NODE => out_node.to_string(),
            *pw::keys::LINK_OUTPUT_PORT => out_port.to_string(),
            *pw::keys::LINK_INPUT_NODE => ours.to_string(),
            *pw::keys::LINK_INPUT_PORT => in_port.to_string(),
            *pw::keys::OBJECT_LINGER => "false",
        };
        let link = core.create_object::<pw::link::Link>("link-factory", &props).map_err(|_| failed(LINK_FAILED))?;
        let key = (out_port, out_node, in_port);
        let listener = link
            .add_listener_local()
            .info({
                let events = Rc::clone(events);
                move |info| {
                    // Each link is listed once, so the list holds at most `graph::MAX_LINKS`.
                    if matches!(info.state(), pw::link::LinkState::Error(_))
                        && let Ok(mut e) = events.try_borrow_mut()
                        && !e.failed_links.contains(&key)
                    {
                        e.failed_links.push(key);
                    }
                }
            })
            .register();
        links.insert((out_port, out_node, in_port), CaptureLink { link, _listener: listener });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::{AppKey, CaptureState, Discovery};

    /// A server path where nothing exists.
    const NO_SERVER: &str = "/nonexistent/studiocraft-test-pipewire-0";
    const WAIT: Duration = Duration::from_secs(10);

    /// A server path that exists but where no server listens: a plain file, made for one test.
    fn silent_server(name: &str) -> String {
        let path = std::env::temp_dir().join(format!("studiocraft-app-audio-{}-{name}", std::process::id()));
        std::fs::write(&path, b"").unwrap();
        path.display().to_string()
    }

    #[test]
    fn the_socket_comes_from_the_environment() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| (*v).to_owned());
        assert_eq!(socket_path(env(&[("XDG_RUNTIME_DIR", "/run/user/1000")])), Some(PathBuf::from("/run/user/1000/pipewire-0")));
        assert_eq!(
            socket_path(env(&[("XDG_RUNTIME_DIR", "/run/user/1000"), ("PIPEWIRE_RUNTIME_DIR", "/pw")])),
            Some(PathBuf::from("/pw/pipewire-0")),
            "PipeWire's own folder comes first"
        );
        assert_eq!(socket_path(env(&[("PIPEWIRE_REMOTE", "/tmp/other"), ("XDG_RUNTIME_DIR", "/run")])), Some(PathBuf::from("/tmp/other")));
        assert_eq!(socket_path(env(&[("PIPEWIRE_REMOTE", "studio"), ("XDG_RUNTIME_DIR", "/run")])), Some(PathBuf::from("/run/studio")));
        assert_eq!(socket_path(env(&[])), None, "no folder to look in");
        assert_eq!(socket_path(env(&[("XDG_RUNTIME_DIR", "")])), None);
        let here = ["abc".to_owned()];
        assert_eq!(pulse_sockets(env(&[("XDG_RUNTIME_DIR", "/run/user/1000")]), &here), [PathBuf::from("/run/user/1000/pulse/native")]);
        assert_eq!(pulse_sockets(env(&[("XDG_RUNTIME_DIR", "/run"), ("PULSE_RUNTIME_PATH", "/p")]), &here), [PathBuf::from("/p/native")]);
        assert_eq!(pulse_sockets(env(&[]), &here), Vec::<PathBuf>::new());
        assert_eq!(
            pulse_sockets(
                env(&[("PULSE_SERVER", "tcp:host:4713 {abc}unix:/srv/pulse {other}/srv/x {abc /tmp/p unix:relative"), ("XDG_RUNTIME_DIR", "/run")]),
                &here
            ),
            [PathBuf::from("/srv/pulse"), PathBuf::from("/tmp/p"), PathBuf::from("/run/pulse/native")],
            "the local servers PULSE_SERVER lists for this machine come first"
        );
    }

    #[test]
    fn a_system_without_the_server_says_why() {
        let s = support_for(None);
        assert!(!s.supported);
        assert_eq!(s.reason.as_deref(), Some(crate::NOT_AVAILABLE));
        // A server, or the folder of its socket, that is not there yet may start later; the list
        // reports it and connects again.
        assert!(LinuxBackend::with_remote(NO_SERVER).support().supported);
    }

    #[test]
    fn only_a_socket_file_is_connected_to() {
        let dir = std::env::temp_dir().join(format!("studiocraft-app-audio-{}-runtime", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!can_connect(None), "no folder to look in");
        assert!(!can_connect(Some(&dir.join("pipewire-0"))), "an empty runtime folder");
        assert!(!can_connect(Some(Path::new(NO_SERVER))), "a missing folder");
        let file = silent_server("plain-file");
        assert!(!can_connect(Some(Path::new(&file))), "a plain file");
        let socket = dir.join("pipewire-0");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        assert!(can_connect(Some(&socket)));
        drop(listener);
        let _ = std::fs::remove_file(socket);
        let _ = std::fs::remove_file(file);
        let _ = std::fs::remove_dir(dir);
    }

    #[test]
    fn samples_keep_whole_frames_and_stop_at_the_limit() {
        let bytes: Vec<u8> = [1.0f32, -1.0, f32::NAN, 0.5, 9.0].iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut pending = Vec::new();
        take_samples(&bytes, 0, 20, false, &mut pending);
        assert_eq!(pending, [1.0, -1.0, 0.0, 0.5], "the fifth sample is half a frame, and NaN is silence");
        pending.clear();
        take_samples(&bytes, 8, 8, false, &mut pending);
        assert_eq!(pending, [0.0, 0.5]);
        take_samples(&bytes, 0, u32::MAX, false, &mut pending);
        assert_eq!(pending.len(), 6, "a size past the buffer reads to its end");
        take_samples(&[], 0, 8, false, &mut pending);
        assert_eq!(pending.len(), 6);
        let ring: Vec<u8> = [1.0f32, 2.0, 3.0, 4.0].iter().flat_map(|s| s.to_le_bytes()).collect();
        pending.clear();
        take_samples(&ring, 8, 16, false, &mut pending);
        assert_eq!(pending, [3.0, 4.0, 1.0, 2.0], "a region past the end goes on from the start");
        pending.clear();
        take_samples(&ring, 24, 8, false, &mut pending);
        assert_eq!(pending, [3.0, 4.0], "an offset past the end is taken modulo the size");
        let mut full = vec![0.0; MAX_PENDING - 2];
        assert_eq!(take_samples(&bytes, 0, 16, false, &mut full), 2, "audio past the limit is dropped and counted");
        assert_eq!(full.len(), MAX_PENDING);
        assert_eq!(take_samples(&bytes, 0, 16, false, &mut full), 4);
        pending.clear();
        assert_eq!(take_samples(&ring, 0, 16, false, &mut pending), 0, "nothing is dropped below the limit");
        // An empty chunk is silence for its frames, whatever its bytes hold.
        pending.clear();
        assert_eq!(take_samples(&ring, 4, 12, true, &mut pending), 0);
        assert_eq!(pending, [0.0, 0.0], "one whole frame of silence");
        pending.clear();
        take_samples(&[], 0, 16, true, &mut pending);
        assert_eq!(pending, [0.0; 4], "an empty chunk without data still counts its frames");
        let mut full = vec![0.0; MAX_PENDING - 2];
        assert_eq!(take_samples(&ring, 0, 16, true, &mut full), 2);
        assert_eq!(full.len(), MAX_PENDING);
    }

    #[test]
    fn packets_carry_their_position_and_the_gap_before_them() {
        let mut clock = Clock::default();
        let mut seen = Vec::new();
        let sounding = |n: usize| Pending { samples: vec![0.25f32; n], runs: vec![(n, false)] };
        clock.deliver(&sounding(PACKET_SAMPLES + 4), 0, |p, info| seen.push((p.len(), info)));
        let frames = (PACKET_SAMPLES / CHANNELS) as u64;
        assert_eq!(
            seen,
            [
                (PACKET_SAMPLES, PacketInfo { position: 0, qpc_100ns: None, discontinuity: false, silent: false }),
                (4, PacketInfo { position: frames, qpc_100ns: None, discontinuity: false, silent: false }),
            ]
        );
        seen.clear();
        // Ten frames dropped after these samples: the next packet starts after them and says so.
        clock.deliver(&sounding(4), 20, |p, info| seen.push((p.len(), info)));
        clock.deliver(&sounding(4), 0, |p, info| seen.push((p.len(), info)));
        clock.deliver(&sounding(4), 0, |p, info| seen.push((p.len(), info)));
        let positions: Vec<(u64, bool)> = seen.iter().map(|(_, i)| (i.position, i.discontinuity)).collect();
        assert_eq!(positions, [(frames + 2, false), (frames + 14, true), (frames + 16, false)]);
        seen.clear();
        // A gap with nothing handed on still marks the next packet.
        clock.deliver(&Pending::default(), 4, |p, info| seen.push((p.len(), info)));
        assert!(seen.is_empty());
        clock.deliver(&sounding(2), 0, |p, info| seen.push((p.len(), info)));
        assert_eq!(seen.first().map(|(_, i)| (i.position, i.discontinuity)), Some((frames + 20, true)));
    }

    #[test]
    fn packets_from_chunks_marked_as_silence_are_silent() {
        let bytes: Vec<u8> = [0.5f32; 8].iter().flat_map(|s| s.to_le_bytes()).collect();
        let mut pending = Pending::default();
        // A whole packet of silence, then half a packet of sound and half of silence.
        for _ in 0..PACKET_SAMPLES / 8 {
            pending.take(&bytes, 0, 32, true);
        }
        for _ in 0..PACKET_SAMPLES / 16 {
            pending.take(&bytes, 0, 32, false);
        }
        for _ in 0..PACKET_SAMPLES / 16 {
            pending.take(&bytes, 0, 32, true);
        }
        pending.take(&bytes, 0, 32, true);
        assert_eq!(pending.runs.len(), 3, "chunks with the same mark share a run");
        let mut silent = Vec::new();
        Clock::default().deliver(&pending, 0, |_, info| silent.push(info.silent));
        assert_eq!(silent, [true, false, true]);
        assert!(!all_silent(&[], 0, 4), "samples outside every run are not silent");
        assert!(!all_silent(&[(4, true)], 2, 2), "an empty range is not silent");
        // Past the run limit, a run counts as silent only if all its chunks are.
        let mut many = Pending::default();
        for i in 0..MAX_RUNS + 4 {
            many.take(&bytes, 0, 8, i % 2 == 0);
        }
        assert_eq!(many.runs.len(), MAX_RUNS);
        assert_eq!(many.runs.last(), Some(&(many.samples.len(), false)));
    }

    #[test]
    fn the_format_parameter_describes_a_pod() {
        let bytes = format_param().unwrap();
        assert!(spa::pod::Pod::from_bytes(&bytes).is_some());
    }

    #[test]
    fn the_list_reports_a_missing_server_without_waiting_on_it() {
        let server = silent_server("list");
        assert!(LinuxBackend::with_remote(&server).support().supported, "the server's socket exists");
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let mut w = LinuxWatcher {
            remote: Some(server.clone()),
            wake: Wake(tx),
            list: None,
            next_start: Instant::now(),
            chosen: BTreeMap::new(),
            cleared: false,
        };
        w.start();
        // No new list thread starts during the test.
        w.next_start = Instant::now() + Duration::from_secs(3600);
        // Each wake is sent before it is looked for, so a missing one fails the test instead of
        // blocking it.
        let thread = w.list.as_mut().and_then(|l| l.thread.take()).expect("a list thread");
        thread.join().expect("the list thread ends");
        assert!(rx.try_recv().is_ok(), "the list thread wakes the discovery when it fails");
        assert!(rx.try_recv().is_err(), "once");
        // The scan after a failure lists nothing and asks for another scan at once, which reports why.
        assert_eq!(w.scan(), Ok(Vec::new()), "a scan never waits on the server");
        assert!(rx.try_recv().is_ok(), "the first scan asks for the next one");
        assert_eq!(w.scan(), Err(AppAudioError::List(NOT_CONNECTED.to_owned())));
        let _ = std::fs::remove_file(server);
    }

    #[test]
    fn a_lost_server_empties_the_list_before_it_reports_why() {
        let (tx, rx) = std::sync::mpsc::sync_channel(4);
        let listed = Arc::new(Mutex::new(Listed::default()));
        let mut w = LinuxWatcher {
            remote: None,
            wake: Wake(tx),
            list: Some(ListThread { listed: Arc::clone(&listed), stop: None, thread: None }),
            // No new list thread starts during the test.
            next_start: Instant::now() + Duration::from_secs(3600),
            chosen: BTreeMap::new(),
            cleared: false,
        };
        let me = std::process::id();
        listed.lock().unwrap().nodes.insert(5, StreamNode { pid: me, running: true });
        assert_eq!(w.scan().unwrap().len(), 1);
        {
            let mut l = listed.lock().unwrap();
            l.nodes.clear();
            l.error = Some(NOT_CONNECTED.to_owned());
        }
        assert_eq!(w.scan(), Ok(Vec::new()), "the first scan after the loss lists nothing");
        assert!(rx.try_recv().is_ok(), "and asks for the next scan at once");
        assert_eq!(w.scan(), Err(AppAudioError::List(NOT_CONNECTED.to_owned())));
        assert_eq!(w.scan(), Err(AppAudioError::List(NOT_CONNECTED.to_owned())));
        // A server that answers again lists its streams, and a later loss empties the list again.
        {
            let mut l = listed.lock().unwrap();
            l.error = None;
            l.nodes.insert(5, StreamNode { pid: me, running: true });
        }
        assert_eq!(w.scan().unwrap().len(), 1);
        listed.lock().unwrap().error = Some(NOT_CONNECTED.to_owned());
        assert_eq!(w.scan(), Ok(Vec::new()));
    }

    #[test]
    fn link_errors_end_the_capture_unless_the_port_went_away() {
        let links = BTreeMap::from([(40, (1, 100, 7)), (41, (2, 100, 8))]);
        assert_eq!(link_errors(&[(40, GONE)], &links), (vec![(1, 100, 7)], None), "a port that went away is forgotten");
        assert_eq!(link_errors(&[(99, -22)], &links), (Vec::new(), None), "an error on another object");
        assert!(matches!(link_errors(&[(41, -13)], &links).1, Some(AppAudioError::Denied(_))));
        assert!(matches!(link_errors(&[(41, -1)], &links).1, Some(AppAudioError::Denied(_))));
        assert_eq!(link_errors(&[(40, GONE), (41, -22)], &links), (vec![(1, 100, 7)], Some(failed(LINK_FAILED))));
        assert_eq!(link_errors(&[], &links), (Vec::new(), None));
    }

    #[test]
    fn a_link_that_keeps_failing_ends_the_capture() {
        let mut failures = BTreeMap::new();
        assert!(!link_failed(&mut failures, (1, 100, 7)), "a port that went away is made again");
        assert!(!link_failed(&mut failures, (2, 100, 8)));
        assert!(!link_failed(&mut failures, (1, 100, 7)));
        assert!(link_failed(&mut failures, (1, 100, 7)), "the third failure of one link ends it");
        // Counts of links the graph no longer needs are forgotten.
        let wanted = BTreeSet::from([(2, 100, 8)]);
        forget_failures(&mut failures, &wanted);
        assert_eq!(failures.keys().copied().collect::<BTreeSet<_>>(), wanted);
    }

    #[test]
    fn a_capture_of_a_process_that_is_gone_is_not_running() {
        let shared = CaptureShared::new(AppKey::from_exe("gone").unwrap(), Target { pid: u32::MAX }, Arc::new(AtomicUsize::new(1)), (1_000, 1_000));
        let mut on_data: OnData = Box::new(|_, _| {});
        let r = LinuxBackend::with_remote(NO_SERVER).capture(Target { pid: u32::MAX }, &shared, &mut on_data);
        assert!(matches!(r, Err(AppAudioError::NotRunning(_))), "{r:?}");
    }

    #[test]
    fn a_capture_without_the_server_fails_at_once() {
        let server = silent_server("capture");
        let d = Discovery::new(Arc::new(LinuxBackend::with_remote(&server)));
        let me = std::process::id();
        let c = d.open_capture(AppKey::from_exe("self").unwrap(), Target { pid: me }, Box::new(|_, _| {})).unwrap();
        assert_eq!(c.wait_ended(WAIT), CaptureState::Failed);
        assert_eq!(c.take_error(), Some(failed(NOT_CONNECTED)));
        let _ = std::fs::remove_file(server);
    }

    #[test]
    fn this_process_has_a_parent_and_a_program_file() {
        let me = std::process::id();
        assert!(ProcFs.exe(me).is_some_and(|p| p.starts_with('/')));
        let parent = ProcFs.parent(me).expect("the test runner has a parent");
        assert!(stat(parent).is_some_and(|(_, start)| stat(me).is_some_and(|(_, mine)| start <= mine)));
        assert!(graph::in_tree(&ProcFs, me, parent));
        assert_eq!(ProcFs.parent(u32::MAX), None);
    }

    #[test]
    fn only_a_removed_program_file_loses_the_deleted_suffix() {
        let dir = std::env::temp_dir().join(format!("studiocraft-app-audio-{}-exe", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A program whose own file name ends in " (deleted)", still at its path.
        let named = dir.join("player (deleted)");
        std::fs::write(&named, b"").unwrap();
        std::os::unix::fs::symlink(&named, dir.join("named-exe")).unwrap();
        assert_eq!(exe_at(&dir.join("named-exe")), Some(named.display().to_string()));
        // A program file that was removed while it runs.
        let removed = dir.join("recorder");
        std::os::unix::fs::symlink(format!("{} (deleted)", removed.display()), dir.join("removed-exe")).unwrap();
        assert_eq!(exe_at(&dir.join("removed-exe")), Some(removed.display().to_string()));
        assert_eq!(exe_at(&dir.join("missing-exe")), None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn streams_take_the_process_id_the_server_read_for_their_client() {
        let me = std::process::id();
        let mut s = Streams::default();
        s.clients.insert(30, me);
        // A client that claims another process, one that claims its own, one whose client the
        // server has not announced, and one with no client.
        s.nodes.insert(100, Followed { client: Some(30), claimed: Some(1), running: true });
        s.nodes.insert(101, Followed { client: Some(30), claimed: Some(me), running: false });
        s.nodes.insert(102, Followed { client: Some(31), claimed: Some(me), running: true });
        s.nodes.insert(103, Followed { client: None, claimed: Some(me), running: true });
        let nodes = s.attributed(&ProcFs);
        assert_eq!(nodes, BTreeMap::from([(100, StreamNode { pid: me, running: true }), (101, StreamNode { pid: me, running: false })]));
        assert!(s.remove(30), "removing a client that owns streams changes them");
        assert!(s.attributed(&ProcFs).is_empty());
        assert!(!s.remove(30));
        assert!(s.remove(100));
        assert!(!s.remove(100));
    }

    /// Answers that every process is the sound server, counts the questions, and lists the
    /// listening sockets it holds.
    struct Asked(Cell<u32>, RefCell<Vec<u64>>);

    impl Procs for Asked {
        fn parent(&self, _: u32) -> Option<u32> {
            None
        }
        fn exe(&self, _: u32) -> Option<String> {
            None
        }
        fn sound_server(&self, _: u32) -> bool {
            self.0.set(self.0.get() + 1);
            true
        }
        fn server_sockets(&self) -> Vec<u64> {
            self.1.borrow().clone()
        }
        fn started(&self, _: u32) -> Option<u64> {
            Some(1)
        }
        fn pulse_connections(&self) -> Option<BTreeSet<u64>> {
            Some(BTreeSet::new())
        }
        fn holds(&self, _: u32, _: &BTreeSet<u64>) -> Option<bool> {
            Some(true)
        }
    }

    /// The sound server is process 7. Each process's start time and the PulseAudio connection
    /// sockets it holds can change; a process missing from `fds` has open files that cannot be
    /// read, and `connections` of `None` is a socket table that cannot be read. Reads of the
    /// socket table are counted.
    #[derive(Default)]
    struct Forwarding {
        starts: RefCell<BTreeMap<u32, u64>>,
        fds: RefCell<BTreeMap<u32, BTreeSet<u64>>>,
        connections: RefCell<Option<BTreeSet<u64>>>,
        reads: Cell<u32>,
        /// A process whose start time changes once it has been asked about.
        ends_when_asked: Cell<Option<u32>>,
    }

    impl Procs for Forwarding {
        fn parent(&self, _: u32) -> Option<u32> {
            None
        }
        fn exe(&self, _: u32) -> Option<String> {
            None
        }
        fn sound_server(&self, pid: u32) -> bool {
            pid == 7
        }
        fn started(&self, pid: u32) -> Option<u64> {
            self.starts.borrow().get(&pid).copied()
        }
        fn pulse_connections(&self) -> Option<BTreeSet<u64>> {
            self.reads.set(self.reads.get() + 1);
            self.connections.borrow().clone()
        }
        fn holds(&self, pid: u32, inodes: &BTreeSet<u64>) -> Option<bool> {
            if self.ends_when_asked.get() == Some(pid) {
                self.starts.borrow_mut().insert(pid, 99);
            }
            self.fds.borrow().get(&pid).map(|held| !held.is_disjoint(inodes))
        }
    }

    #[test]
    fn a_forwarded_stream_is_credited_only_while_its_process_holds_a_pulseaudio_connection() {
        let mut s = Streams::default();
        s.clients.insert(30, 7);
        s.clients.insert(31, 40);
        // Forwarded streams that claim 20, which holds a connection, 21, which holds none, and 22,
        // whose open files cannot be read; and a native client's stream that claims 20.
        s.nodes.insert(100, Followed { client: Some(30), claimed: Some(20), running: true });
        s.nodes.insert(101, Followed { client: Some(30), claimed: Some(21), running: true });
        s.nodes.insert(102, Followed { client: Some(30), claimed: Some(22), running: true });
        s.nodes.insert(103, Followed { client: Some(31), claimed: Some(20), running: false });
        let f = Forwarding::default();
        f.starts.replace(BTreeMap::from([(20, 5), (21, 5), (22, 5), (40, 5)]));
        f.fds.replace(BTreeMap::from([(20, BTreeSet::from([500, 9])), (21, BTreeSet::from([9])), (40, BTreeSet::new())]));
        f.connections.replace(Some(BTreeSet::from([500, 501])));
        let native = || (103, StreamNode { pid: 40, running: false });
        assert_eq!(s.attributed(&f), BTreeMap::from([(100, StreamNode { pid: 20, running: true }), native()]));
        assert_eq!(f.reads.get(), 1, "the socket table is read once for all the streams");
        // Process 20 closes its connection while its stream is still listed.
        f.fds.borrow_mut().insert(20, BTreeSet::from([9]));
        assert_eq!(s.attributed(&f), BTreeMap::from([native()]), "the connection is checked again");
        f.fds.borrow_mut().insert(20, BTreeSet::from([501]));
        assert!(s.attributed(&f).contains_key(&100));
        // The socket table cannot be read.
        f.connections.replace(None);
        assert_eq!(s.attributed(&f), BTreeMap::from([native()]));
        f.connections.replace(Some(BTreeSet::from([501])));
        // A process that is gone is not credited.
        f.starts.borrow_mut().remove(&20);
        assert_eq!(s.attributed(&f), BTreeMap::from([native()]));
        // Process 20 ends while it is checked, and its id goes to a later process.
        f.starts.borrow_mut().insert(20, 5);
        f.ends_when_asked.set(Some(20));
        assert_eq!(s.attributed(&f), BTreeMap::from([native()]), "a process id that changed hands during the check");
    }

    #[test]
    fn a_closed_pulseaudio_connection_is_published_without_a_registry_change() {
        let mut s = Streams::default();
        s.clients.insert(30, 7);
        s.nodes.insert(100, Followed { client: Some(30), claimed: Some(20), running: true });
        let streams = RefCell::new(s);
        let f = Forwarding::default();
        f.starts.replace(BTreeMap::from([(20, 5)]));
        f.fds.replace(BTreeMap::from([(20, BTreeSet::from([500]))]));
        f.connections.replace(Some(BTreeSet::from([500])));
        let listed = Mutex::new(Listed::default());
        assert!(streams.borrow().claims_another(), "the stream claims 20, its client is 7");
        assert!(streams.borrow().needs_reading());
        assert!(republish(&streams, &f, &listed));
        assert_eq!(listed.lock().unwrap().nodes, BTreeMap::from([(100, StreamNode { pid: 20, running: true })]));
        assert!(!republish(&streams, &f, &listed), "an unchanged list is not published again");
        // Process 20 closes its connection; the registry still lists the stream.
        f.fds.borrow_mut().insert(20, BTreeSet::new());
        assert!(republish(&streams, &f, &listed));
        assert!(listed.lock().unwrap().nodes.is_empty());
        // A stream that claims its own client's process, or nothing, is not checked again.
        streams.borrow_mut().nodes.insert(100, Followed { client: Some(30), claimed: Some(7), running: true });
        streams.borrow_mut().nodes.insert(101, Followed { client: Some(30), claimed: None, running: true });
        assert!(!streams.borrow().claims_another());
        assert!(!streams.borrow().needs_reading(), "client 30 is known to be the sound server");
        // A stream of a new client needs a reading until the client is known.
        streams.borrow_mut().clients.insert(31, 40);
        streams.borrow_mut().nodes.insert(102, Followed { client: Some(31), claimed: None, running: true });
        assert!(streams.borrow().needs_reading());
        f.starts.borrow_mut().insert(40, 5);
        republish(&streams, &f, &listed);
        assert!(!streams.borrow().needs_reading());
        assert_eq!(streams.borrow().reading_interval(), Some(SLOW_READINGS), "known clients are read again now and then");
        streams.borrow_mut().nodes.clear();
        assert_eq!(streams.borrow().reading_interval(), None);
    }

    #[test]
    fn the_pulseaudio_sockets_come_from_the_scans_last_reading() {
        let scan = SocketScan {
            latest: Arc::new(ArcSwap::from_pointee(Reading::default())),
            every_ms: Arc::new(AtomicU64::new(0)),
            turned_on: Arc::new(AtomicU64::new(0)),
            wake: None,
            thread: None,
        };
        let procs = Scanned(&scan);
        let reading = Sockets { servers: vec![u64::MAX], connections: Some(BTreeSet::from([u64::MAX - 1])) };
        scan.want(Some(SLOW_READINGS));
        assert!(scan.sockets().is_none(), "nothing before the first reading");
        assert!(scan.waiting());
        scan.latest.store(Arc::new(Reading { count: 1, turned_on: 1, sockets: reading.clone() }));
        assert_eq!(scan.readings(), 1);
        assert!(!scan.waiting());
        assert_eq!(procs.pulse_connections(), reading.connections);
        assert_eq!(procs.server_sockets(), reading.servers);
        assert!(!procs.sound_server(std::process::id()), "this process holds neither socket");
        scan.want(Some(ALIVE_CHECK));
        assert!(!scan.waiting(), "a new interval keeps the reading");
        // With the readings off, the last reading is still used.
        scan.want(None);
        assert!(scan.sockets().is_none());
        assert!(!scan.waiting());
        assert_eq!(procs.pulse_connections(), reading.connections);
        // Turned on again, the readings wait for one that starts afterwards.
        scan.want(Some(ALIVE_CHECK));
        assert!(scan.waiting(), "the reading from before is not used");
        scan.latest.store(Arc::new(Reading { count: 2, turned_on: 2, sockets: reading.clone() }));
        assert_eq!(scan.sockets().map(|s| s.servers), Some(reading.servers.clone()));
        // A running scan ends once it is dropped.
        let running = SocketScan::start().expect("a thread for the scan");
        assert!(running.thread.is_some());
        drop(running);
    }

    #[test]
    fn a_process_holds_a_socket_when_one_of_its_open_files_is_that_socket() {
        let root = std::env::temp_dir().join(format!("studiocraft-app-audio-{}-proc", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let fd = root.join("42").join("fd");
        std::fs::create_dir_all(&fd).unwrap();
        std::os::unix::fs::symlink("socket:[777]", fd.join("3")).unwrap();
        std::os::unix::fs::symlink("/dev/null", fd.join("4")).unwrap();
        std::os::unix::fs::symlink("pipe:[778]", fd.join("5")).unwrap();
        assert_eq!(holds_at(&root, 42, &BTreeSet::from([777])), Some(true));
        assert_eq!(holds_at(&root, 42, &BTreeSet::from([776, 777])), Some(true));
        assert_eq!(holds_at(&root, 42, &BTreeSet::from([778])), Some(false), "a pipe is not a socket");
        assert_eq!(holds_at(&root, 42, &BTreeSet::new()), Some(false));
        assert_eq!(holds_at(&root, 43, &BTreeSet::from([777])), None, "a process whose open files cannot be read");
        assert_eq!(holds_at(&root.join("missing"), 42, &BTreeSet::from([777])), None, "an unreadable process table");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_client_end_of_a_connection_to_a_socket_file_is_found() {
        let me = std::process::id();
        let path = std::env::temp_dir().join(format!("studiocraft-app-audio-{me}-connected"));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert_eq!(connections_to(std::slice::from_ref(&path)), Some(BTreeSet::new()), "no connections yet");
        let client = std::os::unix::net::UnixStream::connect(&path).unwrap();
        let (accepted, _) = listener.accept().unwrap();
        let peers = connections_to(std::slice::from_ref(&path)).unwrap();
        assert_eq!(peers, BTreeSet::from([socket_inode(&client)]), "the client's end, not the server's");
        assert!(!peers.contains(&socket_inode(&accepted)));
        assert_eq!(holds_at(Path::new("/proc"), me, &peers), Some(true));
        assert_eq!(connections_to(&[path.with_extension("other")]), None, "no socket at the path");
        drop((client, accepted, listener));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_sound_server_answer_is_kept_while_its_client_is_listed() {
        let mut s = Streams::default();
        s.clients.insert(30, 7);
        s.clients.insert(31, 7);
        s.nodes.insert(100, Followed { client: Some(30), claimed: Some(1), running: true });
        s.nodes.insert(101, Followed { client: Some(31), claimed: Some(2), running: true });
        let asked = Asked(Cell::new(0), RefCell::new(vec![5]));
        assert_eq!(s.attributed(&asked).len(), 2);
        assert_eq!(s.attributed(&asked).len(), 2);
        assert_eq!(asked.0.get(), 2, "one question for each client, across publications");
        s.remove(30);
        s.attributed(&asked);
        assert_eq!(asked.0.get(), 2);
        // The stream goes first and the client after it, with no publication between.
        s.remove(101);
        s.attributed(&asked);
        s.remove(31);
        s.clients.insert(32, 7);
        s.nodes.insert(102, Followed { client: Some(32), claimed: Some(3), running: true });
        s.attributed(&asked);
        assert_eq!(asked.0.get(), 3, "a process id that comes back with a new client is asked again");
        s.attributed(&asked);
        assert_eq!(asked.0.get(), 3);
        // A new client for the same process id while the earlier one is still listed.
        s.clients.insert(33, 7);
        s.nodes.insert(103, Followed { client: Some(33), claimed: Some(4), running: true });
        s.attributed(&asked);
        assert_eq!(asked.0.get(), 4);
        asked.1.replace(vec![5, 6]);
        s.attributed(&asked);
        assert_eq!(asked.0.get(), 6, "a socket that starts listening clears the answers");
    }

    /// The inode of the socket `fd` refers to.
    fn socket_inode(fd: &impl std::os::fd::AsRawFd) -> u64 {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap().ino()
    }

    #[test]
    fn the_process_that_listens_at_a_socket_holds_it() {
        let me = std::process::id();
        let path = std::env::temp_dir().join(format!("studiocraft-app-audio-{me}-listen"));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let inodes = bound_listeners(std::slice::from_ref(&path));
        assert_eq!(inodes, [socket_inode(&listener)]);
        assert!(holds_any(me, &inodes));
        assert!(bound_listeners(&[path.with_extension("other")]).is_empty());
        assert!(!holds_any(u32::MAX, &inodes));
        assert!(!holds_any(me, &[]));
        drop(listener);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn only_the_socket_the_path_leads_to_now_is_the_servers() {
        let me = std::process::id();
        let path = std::env::temp_dir().join(format!("studiocraft-app-audio-{me}-rebound"));
        let _ = std::fs::remove_file(&path);
        let first = std::os::unix::net::UnixListener::bind(&path).unwrap();
        // The file is removed and another socket is bound at the same path; the first socket still
        // listens and the system's socket table names it by the same path.
        std::fs::remove_file(&path).unwrap();
        assert!(bound_listeners(std::slice::from_ref(&path)).is_empty(), "a socket whose file is gone");
        let second = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert_eq!(bound_listeners(std::slice::from_ref(&path)), [socket_inode(&second)]);
        drop((first, second));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn this_process_is_not_the_sound_server() {
        let me = std::process::id();
        assert!(!ProcFs.sound_server(me));
        assert!(!ProcFs.sound_server(u32::MAX));
        assert!(machine_names().iter().all(|n| !n.is_empty() && n.trim() == n), "names are trimmed and not empty");
    }

    #[test]
    fn a_process_is_known_by_its_id_and_start_time() {
        let me = std::process::id();
        let start = started_at(me).expect("this process has a start time");
        assert!(same_process(me, start));
        assert!(!same_process(me, start + 1), "a later process given the same id is another process");
        assert!(!same_process(u32::MAX, start));
    }
}
