//! The parts of the Linux backend that need no sound server: reading a node's and a port's
//! properties, the process table, the application list built from the playback streams, and the
//! links a capture makes.

use std::collections::{BTreeMap, BTreeSet};

use crate::{AppInfo, AppKey};

/// How far up the parent chain a capture target is looked for.
pub(super) const MAX_PARENT_DEPTH: usize = 16;
/// The most playback streams the backend keeps; streams past this are not listed or captured.
pub(super) const MAX_NODES: usize = 4096;
/// The most ports a capture keeps.
pub(super) const MAX_PORTS: usize = 16_384;
/// The most links one capture makes.
pub(super) const MAX_LINKS: usize = 256;
/// The most clients the backend keeps; streams of clients past this are not listed or captured.
pub(super) const MAX_CLIENTS: usize = 4096;

/// The media class of an application's playback stream.
pub(super) const PLAYBACK_CLASS: &str = "Stream/Output/Audio";

/// One application's playback stream: a node of class [`PLAYBACK_CLASS`] and the process it is
/// attributed to (see [`attribute`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct StreamNode {
    pub pid: u32,
    /// Whether the node runs: a stream is playing, which may be paused or silent.
    pub running: bool,
}

/// Whether a node of this media class is an application's playback stream.
pub(super) fn is_playback(media_class: Option<&str>) -> bool {
    media_class == Some(PLAYBACK_CLASS)
}

/// A process or object id from a property's text. Text that is missing, 0 or not a number gives
/// `None`.
pub(super) fn parse_id(text: Option<&str>) -> Option<u32> {
    text?.trim().parse::<u32>().ok().filter(|p| *p != 0)
}

/// The process a playback stream is attributed to. `real` is the process id the server read from
/// the socket of the client that owns the stream (`pipewire.sec.pid`), which the client cannot
/// set; `claimed` is the `application.process.id` the client wrote into the stream's properties.
///
/// - Without `real`, the stream is attributed to no process.
/// - A client that is the system's own sound server (see [`Procs::sound_server`]) forwards the
///   streams of the applications that connect to its PulseAudio socket, so its streams are
///   attributed to the process they claim; a stream of its own, or one that claims no other
///   process, is attributed to none.
/// - Any other client's streams are attributed to the client's own process, whatever they claim.
pub(super) fn attribute(claimed: Option<u32>, real: Option<u32>, procs: &dyn Procs) -> Option<u32> {
    let real = real.filter(|p| *p != 0)?;
    if procs.sound_server(real) {
        return claimed.filter(|c| *c != real);
    }
    Some(real)
}

/// Which captured channel a port's audio goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Side {
    Left,
    Right,
    /// Both: a mono or center channel, or one the backend does not know.
    Both,
    /// Neither: the low-frequency effects channel.
    Neither,
}

/// The side of a channel position name (`audio.channel`, such as `FL` or `MONO`).
pub(super) fn side(channel: Option<&str>) -> Side {
    match channel.map(str::trim) {
        Some("FL" | "SL" | "RL" | "FLC" | "RLC" | "FLW" | "FLH" | "TFL" | "TRL" | "TSL" | "BLC") => Side::Left,
        Some("FR" | "SR" | "RR" | "FRC" | "RRC" | "FRW" | "FRH" | "TFR" | "TRR" | "TSR" | "BRC") => Side::Right,
        Some("LFE" | "LFE2") => Side::Neither,
        _ => Side::Both,
    }
}

/// One audio port of a node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Port {
    pub node: u32,
    pub output: bool,
    pub side: Side,
}

/// The port a port's properties describe. Monitor ports are left out: they copy a node's input,
/// not what an application plays.
pub(super) fn port(direction: Option<&str>, node: Option<&str>, channel: Option<&str>, monitor: Option<&str>) -> Option<Port> {
    if monitor == Some("true") {
        return None;
    }
    let output = match direction {
        Some("out") => true,
        Some("in") => false,
        _ => return None,
    };
    let node = node?.trim().parse::<u32>().ok()?;
    Some(Port { node, output, side: side(channel) })
}

/// What the process table says about a process.
pub(super) trait Procs {
    /// The parent of `pid`, when it is known and started no later than `pid` did. A process that
    /// started later holds a reused id, not the parent.
    fn parent(&self, pid: u32) -> Option<u32>;
    /// The full path of the program file `pid` runs, when the system shows it.
    fn exe(&self, pid: u32) -> Option<String>;
    /// Whether `pid` is the sound server that forwards PulseAudio clients' streams: the process
    /// that holds the listening socket the user's PulseAudio address leads to now, or a process
    /// that root runs whose program file is named `pipewire` or `pipewire-pulse`.
    fn sound_server(&self, pid: u32) -> bool;
    /// The inodes of the listening sockets the user's PulseAudio addresses lead to now. When they
    /// change, [`Procs::sound_server`] may answer differently.
    fn server_sockets(&self) -> Vec<u64> {
        Vec::new()
    }
}

/// The parent process id and start time from the text of `/proc/<pid>/stat`. The program name in
/// the second field is in parentheses and may hold spaces and parentheses, so the fields are
/// counted from the last `)`.
pub(super) fn parse_stat(text: &str) -> Option<(u32, u64)> {
    let (_, rest) = text.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // Field 3 (the state) is the first after the name; field 4 is the parent, field 22 the start.
    let parent = fields.get(1)?.parse().ok()?;
    let start = fields.get(19)?.parse().ok()?;
    Some((parent, start))
}

/// A listening Unix socket bound to a file: the socket's inode, and the device and inode of the
/// file, as the kernel's socket diagnostics report them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Listener {
    pub socket: u64,
    pub dev: u32,
    pub ino: u32,
}

/// `SOCK_DIAG_BY_FAMILY`, the message type of a socket diagnostics request and its replies.
const SOCK_DIAG_BY_FAMILY: u16 = 20;
/// `NLMSG_ERROR` and `NLMSG_DONE`: a failed request, and the end of a listing.
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
/// `NLM_F_REQUEST | NLM_F_DUMP`.
const DUMP_REQUEST: u16 = 0x0001 | 0x0300;
/// `AF_UNIX`.
const AF_UNIX: u8 = 1;
/// `1 << TCP_LISTEN`: listening sockets only.
const LISTEN_STATES: u32 = 1 << 10;
/// `UDIAG_SHOW_VFS`: the device and inode of the file a socket is bound to.
const SHOW_VFS: u32 = 0x2;
/// `UNIX_DIAG_VFS`, the attribute that holds them.
const UNIX_DIAG_VFS: u16 = 1;
/// The sizes of `struct nlmsghdr`, `struct unix_diag_msg` and `struct rtattr`.
const NLMSG_HEADER: usize = 16;
const DIAG_MSG: usize = 16;
const RTA_HEADER: usize = 4;

/// The request for every listening Unix socket with the file it is bound to: a `struct nlmsghdr`
/// followed by a `struct unix_diag_req` (`linux/sock_diag.h`, `linux/unix_diag.h`).
pub(super) fn diag_request() -> [u8; 40] {
    let mut req = [0u8; 40];
    req[0..4].copy_from_slice(&40u32.to_ne_bytes());
    req[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    req[6..8].copy_from_slice(&DUMP_REQUEST.to_ne_bytes());
    req[8..12].copy_from_slice(&1u32.to_ne_bytes());
    req[16] = AF_UNIX;
    req[20..24].copy_from_slice(&LISTEN_STATES.to_ne_bytes());
    req[28..32].copy_from_slice(&SHOW_VFS.to_ne_bytes());
    req
}

/// What one received part of a socket diagnostics listing says.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Diag {
    /// More parts follow.
    More,
    /// The listing is complete.
    Done,
    /// The kernel refused the request, or the part could not be read.
    Failed,
}

fn u16_at(buf: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_ne_bytes(buf.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(buf: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_ne_bytes(buf.get(at..at + 4)?.try_into().ok()?))
}

/// Adds the bound listening sockets in one received part of a socket diagnostics listing to
/// `found`. A socket that is not bound to a file is skipped.
pub(super) fn parse_diag(buf: &[u8], found: &mut Vec<Listener>) -> Diag {
    let mut at = 0;
    while at < buf.len() {
        let (Some(len), Some(kind)) = (u32_at(buf, at), u16_at(buf, at + 4)) else { return Diag::Failed };
        let Ok(len) = usize::try_from(len) else { return Diag::Failed };
        if len < NLMSG_HEADER || at + len > buf.len() {
            return Diag::Failed;
        }
        match kind {
            NLMSG_DONE => return Diag::Done,
            NLMSG_ERROR => return Diag::Failed,
            SOCK_DIAG_BY_FAMILY => {
                let msg = &buf[at + NLMSG_HEADER..at + len];
                let Some(socket) = u32_at(msg, 4) else { return Diag::Failed };
                let mut a = DIAG_MSG;
                while a + RTA_HEADER <= msg.len() {
                    let (Some(rta_len), Some(rta_kind)) = (u16_at(msg, a), u16_at(msg, a + 2)) else { break };
                    let rta_len = usize::from(rta_len);
                    if rta_len < RTA_HEADER || a + rta_len > msg.len() {
                        break;
                    }
                    if rta_kind == UNIX_DIAG_VFS
                        && let (Some(ino), Some(dev)) = (u32_at(msg, a + RTA_HEADER), u32_at(msg, a + RTA_HEADER + 4))
                    {
                        found.push(Listener { socket: u64::from(socket), dev, ino });
                    }
                    a += rta_len.next_multiple_of(4);
                }
            }
            _ => {}
        }
        at += len.next_multiple_of(4);
    }
    Diag::More
}

/// The device number in the kernel's own encoding (major in the top 12 bits, minor in the low
/// 20), which the socket diagnostics report, from the encoding `stat` reports.
pub(super) fn kernel_dev(st_dev: u64) -> u64 {
    let major = ((st_dev & 0x0000_0000_000f_ff00) >> 8) | ((st_dev & 0xffff_f000_0000_0000) >> 32);
    let minor = (st_dev & 0xff) | ((st_dev & 0x0000_0fff_fff0_0000) >> 12);
    (major << 20) | minor
}

/// The listening socket bound to the file with device `st_dev` and inode `st_ino`, as `stat`
/// reports them. The diagnostics hold the low 32 bits of the inode.
pub(super) fn bound_to(listeners: &[Listener], st_dev: u64, st_ino: u64) -> Option<u64> {
    let dev = kernel_dev(st_dev);
    let ino = st_ino & u64::from(u32::MAX);
    listeners.iter().find(|l| u64::from(l.dev) == dev && u64::from(l.ino) == ino).map(|l| l.socket)
}

/// The file name of a program path, or of a program name.
fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Whether process `pid` is `target` or runs under it.
pub(super) fn in_tree(procs: &dyn Procs, pid: u32, target: u32) -> bool {
    let mut at = pid;
    for _ in 0..=MAX_PARENT_DEPTH {
        if at == target {
            return true;
        }
        match procs.parent(at) {
            Some(p) => at = p,
            None => return false,
        }
    }
    false
}

/// The links a capture of `target` needs, as `(output port, output node, input port)`: every
/// output port of a playback stream that `target` or one of its child processes plays, to each
/// input port of `ours` on the same side. At most [`MAX_LINKS`], lowest port ids first.
pub(super) fn plan(
    target: u32,
    nodes: &BTreeMap<u32, StreamNode>,
    ports: &BTreeMap<u32, Port>,
    ours: u32,
    procs: &dyn Procs,
) -> BTreeSet<(u32, u32, u32)> {
    let inputs: Vec<(u32, Side)> = ports.iter().filter(|(_, p)| p.node == ours && !p.output).map(|(id, p)| (*id, p.side)).collect();
    // Streams of one process share its answer, so each process's parent chain is read once.
    let mut tree: BTreeMap<u32, bool> = BTreeMap::new();
    let captured: BTreeSet<u32> = nodes
        .iter()
        .filter(|(id, n)| **id != ours && *tree.entry(n.pid).or_insert_with(|| in_tree(procs, n.pid, target)))
        .map(|(id, _)| *id)
        .collect();
    let mut out = BTreeSet::new();
    for (id, p) in ports.iter().filter(|(_, p)| p.output && captured.contains(&p.node)) {
        for (input, input_side) in &inputs {
            let joins = match (p.side, input_side) {
                (Side::Neither, _) => false,
                (Side::Both, _) => true,
                (from, to) => from == *to,
            };
            if joins && out.len() < MAX_LINKS {
                out.insert((*id, p.node, *input));
            }
        }
    }
    out
}

/// A program name and its path: one listed application.
pub(super) type Group = (String, String);

/// One process tree of an application: whether one of its streams runs.
#[derive(Clone, Copy, Default)]
struct Tree {
    running: bool,
}

/// The tree a capture of one application takes. The tree taken last time stays while one of its
/// streams runs, or while no other tree's does, so two instances whose streams start and stop do
/// not make the source capture again and again. Otherwise a tree with a running stream, then the
/// lowest process id.
fn choose(trees: &BTreeMap<u32, Tree>, previous: Option<u32>) -> Option<u32> {
    if let Some(p) = previous
        && let Some(t) = trees.get(&p)
        && (t.running || !trees.iter().any(|(pid, other)| *pid != p && other.running))
    {
        return Some(p);
    }
    trees.iter().max_by_key(|(pid, t)| (t.running, std::cmp::Reverse(**pid))).map(|(pid, _)| *pid)
}

/// The topmost process above `pid` that runs the program file at `path`. Without a path, `pid`
/// itself.
fn target(procs: &dyn Procs, pid: u32, path: &str) -> u32 {
    let mut top = pid;
    if path.is_empty() {
        return top;
    }
    for _ in 0..MAX_PARENT_DEPTH {
        match procs.parent(top) {
            Some(p) if procs.exe(p).is_some_and(|e| e == path) => top = p,
            _ => break,
        }
    }
    top
}

/// The first of `targets` above `pid` in its parent chain.
fn inside(procs: &dyn Procs, pid: u32, targets: &BTreeMap<u32, String>) -> Option<String> {
    let mut at = pid;
    for _ in 0..MAX_PARENT_DEPTH {
        at = procs.parent(at)?;
        if let Some(choice) = targets.get(&at) {
            return Some(choice.clone());
        }
    }
    None
}

/// The applications the playback streams belong to, and the tree each one's capture takes. An
/// application is named by its process's program file, never by a name the stream reports or by
/// the command name a process sets for itself; a stream whose program file the system does not
/// show is left out. `previous` holds the trees the last list chose.
pub(super) fn apps(nodes: &BTreeMap<u32, StreamNode>, procs: &dyn Procs, previous: &BTreeMap<Group, u32>) -> (Vec<AppInfo>, BTreeMap<Group, u32>) {
    // Each stream's application, path and capture target.
    let mut rows: Vec<(Group, AppKey, String, String, u32, &StreamNode)> = Vec::new();
    for n in nodes.values() {
        let Some(path) = procs.exe(n.pid).filter(|p| !p.is_empty()) else { continue };
        let name = file_name(&path).to_owned();
        // `AppKey` reads a `\` as a folder separator, so a Linux file name with one gets no key.
        if file_name(&name).contains('\\') {
            continue;
        }
        let Some(key) = AppKey::from_exe(file_name(&name)) else { continue };
        let target = target(procs, n.pid, &path);
        // Linux paths keep their letter case: two programs whose paths differ only in case are
        // two applications.
        rows.push(((key.name.clone(), path.clone()), key, file_name(&name).to_owned(), path, target, n));
    }
    let mut trees: BTreeMap<Group, BTreeMap<u32, Tree>> = BTreeMap::new();
    for (group, _, _, _, target, n) in &rows {
        trees.entry(group.clone()).or_default().entry(*target).or_default().running |= n.running;
    }
    let mut chosen: BTreeMap<Group, u32> = BTreeMap::new();
    let mut listed: BTreeMap<Group, AppInfo> = BTreeMap::new();
    for (group, key, name, path, _, n) in rows {
        let target =
            *chosen.entry(group.clone()).or_insert_with(|| trees.get(&group).and_then(|t| choose(t, previous.get(&group).copied())).unwrap_or(n.pid));
        let entry = listed.entry(group).or_insert_with(|| AppInfo {
            key,
            name,
            path,
            pids: Vec::new(),
            target_pid: target,
            playing: false,
            within: None,
            slot: None,
            key_id: 0,
        });
        if !entry.pids.contains(&n.pid) {
            entry.pids.push(n.pid);
        }
        entry.playing |= n.running;
    }
    // An application whose target runs under another listed application's target has that tree
    // recorded by the other application's capture too.
    let by_target: BTreeMap<u32, String> = listed.values().map(|a| (a.target_pid, a.choice())).collect();
    let mut list: Vec<AppInfo> = listed.into_values().collect();
    for a in &mut list {
        a.pids.sort_unstable();
        a.within = inside(procs, a.target_pid, &by_target);
    }
    (list, chosen)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A process table: each process's parent and program path, and the processes that run the
    /// sound server.
    #[derive(Default)]
    struct Table {
        procs: BTreeMap<u32, (Option<u32>, &'static str)>,
        servers: BTreeSet<u32>,
    }

    impl Table {
        fn with(mut self, pid: u32, parent: Option<u32>, exe: &'static str) -> Table {
            self.procs.insert(pid, (parent, exe));
            self
        }

        fn server(mut self, pid: u32) -> Table {
            self.servers.insert(pid);
            self
        }
    }

    impl Procs for Table {
        fn parent(&self, pid: u32) -> Option<u32> {
            self.procs.get(&pid).and_then(|(p, _)| *p)
        }

        fn exe(&self, pid: u32) -> Option<String> {
            self.procs.get(&pid).map(|(_, e)| (*e).to_owned()).filter(|e| !e.is_empty())
        }

        fn sound_server(&self, pid: u32) -> bool {
            self.servers.contains(&pid)
        }
    }

    fn node(pid: u32, running: bool) -> StreamNode {
        StreamNode { pid, running }
    }

    #[test]
    fn only_playback_streams_and_real_ids_are_read() {
        assert!(is_playback(Some(PLAYBACK_CLASS)));
        assert!(!is_playback(Some("Stream/Input/Audio")), "a recording stream");
        assert!(!is_playback(Some("Audio/Sink")), "a device");
        assert!(!is_playback(None));
        assert_eq!(parse_id(Some(" 42 ")), Some(42));
        assert_eq!(parse_id(None), None);
        assert_eq!(parse_id(Some("0")), None);
        assert_eq!(parse_id(Some("-5")), None);
        assert_eq!(parse_id(Some("99999999999")), None);
        assert_eq!(parse_id(Some("player")), None);
    }

    #[test]
    fn a_stream_is_attributed_to_the_client_the_server_saw() {
        let t = Table::default().with(10, Some(1), "/usr/bin/player").with(20, Some(1), "/usr/bin/browser");
        assert_eq!(attribute(Some(10), Some(10), &t), Some(10), "a client that claims its own process");
        assert_eq!(attribute(None, Some(10), &t), Some(10), "a client that claims nothing");
        assert_eq!(attribute(Some(20), Some(10), &t), Some(10), "a client that claims another process gets its own");
        assert_eq!(attribute(Some(10), None, &t), None, "without the server's process id nothing is attributed");
        assert_eq!(attribute(Some(10), Some(0), &t), None);
    }

    #[test]
    fn the_sound_servers_streams_are_attributed_to_the_process_they_claim() {
        let t = Table::default().with(5, Some(1), "/usr/bin/pipewire-pulse").server(5).with(20, Some(1), "/usr/bin/browser");
        assert_eq!(attribute(Some(20), Some(5), &t), Some(20), "a stream forwarded from the PulseAudio socket");
        assert_eq!(attribute(None, Some(5), &t), None, "the server's stream without a process");
        assert_eq!(attribute(Some(5), Some(5), &t), None, "the server's own stream");
    }

    #[test]
    fn an_application_is_named_by_its_program_file_only() {
        let t = Table::default().with(10, Some(1), "/usr/bin/player").with(11, Some(1), "").with(12, Some(1), "");
        let t = t.with(13, Some(1), "/opt/foo\\bar");
        let nodes = BTreeMap::from([(100, node(10, true)), (101, node(11, true)), (102, node(12, true)), (103, node(13, true))]);
        let (list, _) = apps(&nodes, &t, &BTreeMap::new());
        let names: Vec<&str> = list.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["player"], "a process whose program file cannot be read, or with a backslash in its name, is left out");
        assert_eq!(list[0].pids, [10], "a process that names itself after a listed program is not added to it");
    }

    #[test]
    fn channels_go_to_their_side() {
        assert_eq!(side(Some("FL")), Side::Left);
        assert_eq!(side(Some("RR")), Side::Right);
        assert_eq!(side(Some("SL")), Side::Left);
        assert_eq!(side(Some("MONO")), Side::Both);
        assert_eq!(side(Some("FC")), Side::Both);
        assert_eq!(side(Some("AUX7")), Side::Both, "an unknown channel is heard on both sides");
        assert_eq!(side(None), Side::Both);
        assert_eq!(side(Some("LFE")), Side::Neither);
    }

    #[test]
    fn ports_need_a_direction_and_a_node_and_skip_monitors() {
        assert_eq!(port(Some("out"), Some("12"), Some("FL"), None), Some(Port { node: 12, output: true, side: Side::Left }));
        assert_eq!(port(Some("in"), Some("12"), Some("FR"), Some("false")), Some(Port { node: 12, output: false, side: Side::Right }));
        assert_eq!(port(Some("out"), Some("12"), Some("FL"), Some("true")), None);
        assert_eq!(port(Some("sideways"), Some("12"), None, None), None);
        assert_eq!(port(Some("out"), Some("twelve"), None, None), None);
        assert_eq!(port(None, Some("12"), None, None), None);
    }

    /// One reply message for a listening socket with inode `socket`, and the file it is bound to
    /// when `vfs` holds its inode and device.
    fn diag_msg(socket: u32, vfs: Option<(u32, u32)>) -> Vec<u8> {
        let mut body = vec![AF_UNIX, 1, 10, 0];
        body.extend_from_slice(&socket.to_ne_bytes());
        body.extend_from_slice(&[0; 8]);
        // An attribute the reader skips: UNIX_DIAG_NAME with a 3-byte name, padded to 4.
        body.extend_from_slice(&7u16.to_ne_bytes());
        body.extend_from_slice(&0u16.to_ne_bytes());
        body.extend_from_slice(b"abc\0");
        if let Some((ino, dev)) = vfs {
            body.extend_from_slice(&12u16.to_ne_bytes());
            body.extend_from_slice(&UNIX_DIAG_VFS.to_ne_bytes());
            body.extend_from_slice(&ino.to_ne_bytes());
            body.extend_from_slice(&dev.to_ne_bytes());
        }
        header(SOCK_DIAG_BY_FAMILY, body)
    }

    fn header(kind: u16, body: Vec<u8>) -> Vec<u8> {
        let len = u32::try_from(NLMSG_HEADER + body.len()).unwrap();
        let mut msg = len.to_ne_bytes().to_vec();
        msg.extend_from_slice(&kind.to_ne_bytes());
        msg.extend_from_slice(&[0; 10]);
        msg.extend(body);
        msg
    }

    #[test]
    fn the_request_asks_for_listening_unix_sockets_with_their_files() {
        let req = diag_request();
        assert_eq!(u32_at(&req, 0), Some(40));
        assert_eq!(u16_at(&req, 4), Some(SOCK_DIAG_BY_FAMILY));
        assert_eq!(u16_at(&req, 6), Some(0x0301));
        assert_eq!(req[16], AF_UNIX);
        assert_eq!(u32_at(&req, 20), Some(1 << 10));
        assert_eq!(u32_at(&req, 28), Some(SHOW_VFS));
    }

    #[test]
    fn the_reply_lists_sockets_bound_to_files() {
        let mut buf = diag_msg(41000, Some((900, 45)));
        buf.extend(diag_msg(41001, None));
        buf.extend(diag_msg(41002, Some((901, 46))));
        let mut found = Vec::new();
        assert_eq!(parse_diag(&buf, &mut found), Diag::More);
        assert_eq!(
            found,
            [Listener { socket: 41000, dev: 45, ino: 900 }, Listener { socket: 41002, dev: 46, ino: 901 }],
            "a socket with no file is skipped"
        );
        assert_eq!(parse_diag(&header(NLMSG_DONE, vec![0; 4]), &mut found), Diag::Done);
        assert_eq!(parse_diag(&header(NLMSG_ERROR, vec![0; 20]), &mut found), Diag::Failed);
        assert_eq!(parse_diag(&buf[..buf.len() - 1], &mut Vec::new()), Diag::Failed, "a cut message");
        assert_eq!(parse_diag(&[0; 3], &mut Vec::new()), Diag::Failed);
        assert_eq!(parse_diag(&[], &mut Vec::new()), Diag::More);
        let mut short = header(SOCK_DIAG_BY_FAMILY, vec![0; 4]);
        short[0] = 8;
        assert_eq!(parse_diag(&short, &mut Vec::new()), Diag::Failed, "a length shorter than the header");
    }

    #[test]
    fn a_socket_is_matched_to_its_file_by_device_and_inode() {
        // Device 8:1 reads as 0x801 from `stat` and as 8 << 20 | 1 in the kernel's encoding;
        // 259:65537 has a major above 255 and a minor above 255.
        assert_eq!(kernel_dev(0x801), (8 << 20) | 1);
        assert_eq!(kernel_dev(0x1001_0301), (259 << 20) | 65537);
        assert_eq!(kernel_dev(45), 45);
        let listeners = [Listener { socket: 7, dev: 45, ino: 900 }, Listener { socket: 8, dev: 46, ino: 900 }];
        assert_eq!(bound_to(&listeners, 46, 900), Some(8));
        assert_eq!(bound_to(&listeners, 45, (1 << 32) | 900), Some(7), "the diagnostics hold the low 32 bits");
        assert_eq!(bound_to(&listeners, 45, 901), None);
        assert_eq!(bound_to(&listeners, 47, 900), None);
    }

    #[test]
    fn stat_fields_are_counted_after_the_name() {
        let stat = "4242 (a (strange) name) S 17 4242 4242 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 98765 1 2 3";
        assert_eq!(parse_stat(stat), Some((17, 98765)));
        assert_eq!(parse_stat("4242 (short) S"), None, "a truncated line");
        assert_eq!(parse_stat("no parentheses at all"), None);
        assert_eq!(parse_stat(""), None);
    }

    #[test]
    fn the_tree_holds_child_processes_only() {
        let t = Table::default().with(10, Some(1), "/usr/bin/player").with(11, Some(10), "/usr/bin/helper").with(12, Some(11), "/usr/bin/x");
        assert!(in_tree(&t, 12, 10));
        assert!(in_tree(&t, 10, 10));
        assert!(!in_tree(&t, 10, 11), "a parent is not inside its child's tree");
        assert!(!in_tree(&t, 99, 10), "an unknown process");
        // A parent chain that loops ends at the depth limit.
        let looped = Table::default().with(20, Some(21), "a").with(21, Some(20), "b");
        assert!(!in_tree(&looped, 20, 30));
    }

    #[test]
    fn a_capture_links_every_stream_of_the_tree_by_side() {
        let t = Table::default().with(10, Some(1), "/usr/bin/browser").with(11, Some(10), "/usr/bin/browser").with(30, Some(1), "/usr/bin/other");
        let nodes = BTreeMap::from([(100, node(10, true)), (101, node(11, true)), (102, node(30, true))]);
        let ours = 200;
        let ports = BTreeMap::from([
            (1, Port { node: 100, output: true, side: Side::Left }),
            (2, Port { node: 100, output: true, side: Side::Right }),
            (3, Port { node: 101, output: true, side: Side::Both }),
            (4, Port { node: 101, output: true, side: Side::Neither }),
            (5, Port { node: 102, output: true, side: Side::Left }),
            (6, Port { node: 100, output: false, side: Side::Left }),
            (7, Port { node: ours, output: false, side: Side::Left }),
            (8, Port { node: ours, output: false, side: Side::Right }),
        ]);
        let links = plan(10, &nodes, &ports, ours, &t);
        assert_eq!(links, BTreeSet::from([(1, 100, 7), (2, 100, 8), (3, 101, 7), (3, 101, 8)]));
        assert!(plan(10, &nodes, &ports, 999, &t).is_empty(), "no input ports yet");
        assert!(plan(77, &nodes, &ports, ours, &t).is_empty(), "a target without streams");
    }

    #[test]
    fn a_capture_makes_no_more_than_the_link_limit() {
        let t = Table::default().with(10, None, "/usr/bin/many");
        let nodes: BTreeMap<u32, StreamNode> = (0..300).map(|i| (1000 + i, node(10, true))).collect();
        let mut ports: BTreeMap<u32, Port> = (0..300).map(|i| (i, Port { node: 1000 + i, output: true, side: Side::Both })).collect();
        ports.insert(5000, Port { node: 1, output: false, side: Side::Left });
        ports.insert(5001, Port { node: 1, output: false, side: Side::Right });
        assert_eq!(plan(10, &nodes, &ports, 1, &t).len(), MAX_LINKS);
    }

    #[test]
    fn streams_are_listed_as_applications_with_their_capture_tree() {
        let t = Table::default()
            .with(10, Some(1), "/usr/lib/browser/browser")
            .with(11, Some(10), "/usr/lib/browser/browser")
            .with(12, Some(10), "/usr/lib/browser/browser")
            .with(20, Some(1), "/usr/bin/player")
            .with(30, Some(10), "/usr/bin/helper")
            .with(40, Some(1), "");
        let nodes =
            BTreeMap::from([(100, node(11, false)), (101, node(12, true)), (102, node(20, false)), (103, node(30, true)), (104, node(40, true))]);
        let (list, chosen) = apps(&nodes, &t, &BTreeMap::new());
        let names: Vec<&str> = list.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["browser", "helper", "player"], "a process with no program name is left out");
        let browser = &list[0];
        assert_eq!(browser.pids, [11, 12]);
        assert_eq!(browser.target_pid, 10, "the topmost process with the same program");
        assert!(browser.playing);
        assert_eq!(browser.path, "/usr/lib/browser/browser");
        assert_eq!(list[1].within.as_deref(), Some(browser.choice().as_str()), "the helper runs inside the browser's tree");
        assert_eq!(list[2].key.name, "player", "named by the program file");
        assert!(!list[2].playing);
        assert_eq!(chosen.get(&("browser".to_owned(), "/usr/lib/browser/browser".to_owned())), Some(&10));
    }

    #[test]
    fn programs_whose_paths_differ_in_case_are_two_applications() {
        let t = Table::default().with(10, Some(1), "/Apps/player").with(20, Some(1), "/apps/player");
        let nodes = BTreeMap::from([(100, node(10, true)), (101, node(20, true))]);
        let (list, _) = apps(&nodes, &t, &BTreeMap::new());
        let targets: Vec<(u32, &str)> = list.iter().map(|a| (a.target_pid, a.path.as_str())).collect();
        assert_eq!(targets, [(10, "/Apps/player"), (20, "/apps/player")]);
        // A program that starts another program with the same file name from another folder:
        // capturing the child does not take in the parent's tree.
        let t = Table::default().with(10, Some(1), "/opt/a/player").with(11, Some(10), "/opt/b/player").with(12, Some(11), "/opt/b/player");
        let nodes = BTreeMap::from([(100, node(10, true)), (101, node(12, true))]);
        let (list, _) = apps(&nodes, &t, &BTreeMap::new());
        let targets: Vec<(u32, &str)> = list.iter().map(|a| (a.target_pid, a.path.as_str())).collect();
        assert_eq!(targets, [(10, "/opt/a/player"), (11, "/opt/b/player")]);
    }

    #[test]
    fn separate_instances_keep_the_tree_chosen_before() {
        let t = Table::default().with(10, Some(1), "/usr/bin/player").with(20, Some(1), "/usr/bin/player");
        let group = ("player".to_owned(), "/usr/bin/player".to_owned());
        let both_idle = BTreeMap::from([(100, node(10, false)), (101, node(20, false))]);
        let (list, chosen) = apps(&both_idle, &t, &BTreeMap::new());
        assert_eq!(list[0].target_pid, 10, "the lowest process id when nothing runs");
        let second_runs = BTreeMap::from([(100, node(10, false)), (101, node(20, true))]);
        let (list, chosen) = apps(&second_runs, &t, &chosen);
        assert_eq!(list[0].target_pid, 20, "a running stream wins over an idle tree chosen before");
        let both_run = BTreeMap::from([(100, node(10, true)), (101, node(20, true))]);
        let (list, _) = apps(&both_run, &t, &chosen);
        assert_eq!(list[0].target_pid, 20, "the tree chosen before stays while it runs");
        assert_eq!(chosen.get(&group), Some(&20));
        let (list, _) = apps(&both_idle, &t, &chosen);
        assert_eq!(list[0].target_pid, 20, "the tree chosen before stays while no other tree runs");
    }

    #[test]
    fn no_streams_list_no_applications() {
        let (list, chosen) = apps(&BTreeMap::new(), &Table::default(), &BTreeMap::new());
        assert!(list.is_empty() && chosen.is_empty());
    }
}
