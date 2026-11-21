# craft-app-audio

Per-application audio for a Craft app's mixer: the list of running applications that have audio,
a peak meter for each one without capturing it, and the capture of one application's audio as
48 kHz stereo `f32` samples. The crate depends on no app crate. It builds on every system and
reports capture as unsupported, with a reason, where the system has no way to do it.

| Item | What |
|---|---|
| `Discovery` | the list of applications, kept current by one thread while anything holds a `DiscoveryUse`, and the captures; `Discovery::system()` is the process's one discovery on the system's backend |
| `AppList`, `AppInfo` | one scan's applications, sorted by name: key, name, program path, process ids, the process a capture targets, whether it plays sound, the listed application whose capture already records it, its peak row |
| `AppKey` | an application named so the name survives a restart with a new process id: the program file's name in lower case, with its path in a list only to tell apart two applications whose files have the same name (in lower case for a Windows path, and in its own letter case for a path that starts with `/`); `AppInfo::choice` saves the name with the path, and `AppList::find` takes the application at that path, or else the only one with that file name unless its path differs from a `/` path only in letter case or trailing spaces, and never guesses between two |
| `PeakTable` | 64 rows of atomics with each listed application's key id, peak level and reading time, read without a lock; a reading older than 250 ms reads as none |
| `AppCapture` | one capture: its state (starting, started, running, failed, ended), its error, a stall check, and its counters, including the packets that followed lost audio; dropping it stops the thread without waiting for it |
| `PacketInfo` | what the callback gets with each packet: the position of its first frame in frames, the performance counter time of that frame in 100 ns units (`None` when the system marks the time uncertain), and whether the system lost audio before the packet or marked it silent |
| `Backoff` | the waits before opening a failed capture again: 250 ms, 500 ms, 1 s, 2 s, then 5 s |
| `Backend`, `Watcher` | the system's side: support, listing and meters, and the capture loop |
| `fake::FakeBackend` | a scripted backend for tests: applications start and quit, captures deliver, lose audio, fail, stall or end |

## Systems

| System | Listing | Meters | Capture |
|---|---|---|---|
| Windows build 20348 and later (Windows 11, Windows Server 2022) | The audio sessions of every active playback endpoint ([audio sessions](https://learn.microsoft.com/en-us/windows/win32/coreaudio/audio-sessions)) | `IAudioMeterInformation` peak values | Process loopback with the process's child processes ([`AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS`](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params), [`ActivateAudioInterfaceAsync`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-activateaudiointerfaceasync)) |
| Earlier Windows builds | Unsupported | Unsupported | Unsupported |
| Linux with the PipeWire server | The nodes of class `Stream/Output/Audio` in the server's registry, each attributed to the process of the client that owns it (`pipewire.sec.pid`) ([objects](https://docs.pipewire.org/page_objects_design.html), [keys](https://docs.pipewire.org/group__pw__keys.html)) | None: the list has no peak readings | A capture stream that the session manager leaves unconnected, linked through `link-factory` from every output port of the process tree's playback streams ([link factory](https://docs.pipewire.org/page_module_link_factory.html)) |
| Linux without the folder of the PipeWire server's socket (`PIPEWIRE_RUNTIME_DIR` or `XDG_RUNTIME_DIR`) | Unsupported | Unsupported | Unsupported |
| macOS | Unsupported | Unsupported | Unsupported |

On Windows the key is the program file's name. A capture targets the topmost process in the
parent chain whose file has the same name, and includes that process's child processes, so an
application that plays from several processes is captured whole. When separate instances of one
program run, each with its own process tree, the list shows them as one application and a
capture takes one tree. The tree taken last time stays while its meter reads sound, or while no
other tree's does. Otherwise the capture takes a tree whose meter read sound within the last 3
scans, then one with an active audio session, then the lowest process id. An active session has a
running stream, which may be paused or silent. A source captures again when that choice changes.
Two instances that play at once are not captured together. An application's meter reads only the
sessions of the tree its capture takes. An application whose target runs under another listed
application's target names that application in `within`. Sessions shared by several processes and
the system sounds session are not listed.

An activation the system refuses (`E_ACCESSDENIED`) is reported as `AppAudioError::Denied`, and an
activation whose target process has exited as `NotRunning`.

On Linux each playback stream is attributed to the process the server read from the socket of the
client that owns it (`pipewire.sec.pid` on the client object), which the client cannot set. A
stream's own `application.process.id` is used only when its client is the sound server that
forwards the streams of applications that use the PulseAudio interface: the process that holds
the socket listening at a local address in `PULSE_SERVER` or at `native` in `PULSE_RUNTIME_PATH`
or `$XDG_RUNTIME_DIR/pulse` (found
through `/proc/net/unix` and `/proc/<pid>/fd`), or a process that root runs (the owner of
`/proc/<pid>`) whose program file is named `pipewire` or `pipewire-pulse`. The sound server does not check those claims, so such a
stream can name another process. A stream whose client has no
`pipewire.sec.pid` is not listed. The key is the file name of `/proc/<pid>/exe`, or else the
command name in `/proc/<pid>/comm`, and the path is the program file's path. Names the stream
reports are not used. A process that shows neither name, or whose name holds a `|` (the
separator in a saved choice) or a `\` (read as a folder separator in a saved choice), is not listed. The capture target and `within` follow the parent chain in `/proc/<pid>/stat`, as on
Windows; the target is the topmost process in the chain that runs the same program file, path
included. Without meters, separate instances are chosen by their streams: the tree taken last time
stays while one of its streams runs, or while no other tree's does; otherwise a tree with a running
stream, then the lowest process id. A capture links each output port to the capture stream's left
input, right input or both, by its channel position: left positions to the left, right positions
to the right, mono, center and unknown positions to both, and the low-frequency effects channel to
neither. Links to playback streams that start during the capture are made as they appear, at most
256 per capture. The capture removes each link from the server when its stream leaves the process
tree and when the capture ends. The application still plays on its own device. A link the server
refuses (`EPERM` or `EACCES`) is reported as `AppAudioError::Denied`. A link whose port went away is
dropped, and a link that fails after the server made it is made again; the third failure of one
link is reported as `AppAudioError::Failed`. A target process that
has exited, at the start or later, as `NotRunning`. The backend is supported when the environment
names the server's socket: `PIPEWIRE_REMOTE`, or `pipewire-0`, in `PIPEWIRE_RUNTIME_DIR` or
`XDG_RUNTIME_DIR`. A missing socket or a server that does not answer is reported in the list's
error, and the list connects again every 2 s.

## Threads

- **Discovery.** One thread per `Discovery`, started by the first `acquire` and stopped when the
  last `DiscoveryUse` is dropped. It lists the applications every 1 s, and at once when an
  endpoint is added, removed or changed or a session changes state (on Linux, when a playback
  stream changes), at most every 250 ms. A thread
  stuck in one system call for more than 5 s shows in `stalled()`, and `recover()` starts one new
  thread beside it; no more than two discovery threads are alive at once. Each list that differs from
  the one before is published as an `Arc<AppList>` with a generation number through an `ArcSwap`:
  readers compare `generation()`, an atomic, and load the list without a lock when it changed.
  While `set_visible(true)` is in force, and for the time given to each `read_peaks_for` call, it
  reads every application's meter about 30 times a second into the `PeakTable`. It reads the
  meters without holding a lock. `Discovery::peak` gives `None` for an application without a
  current reading, which is not the same as silence. Commands reach it through a queue of 16; a
  full queue drops the command. `fresh_list` with a zero timeout never waits.
- **Capture.** One thread per `AppCapture`. It waits for the system's packets at most 200 ms at a
  time and reports a heartbeat each time, converts each packet into a buffer it reuses, and hands
  the samples to the caller's callback with the packet's `PacketInfo`. On Windows the position,
  time and flags are the ones
  [`IAudioCaptureClient::GetBuffer`](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudiocaptureclient-getbuffer)
  returns. After the first packets it does not allocate unless a
  packet is larger than any before. `stalled(now)` is true when an activation has not returned for
  10 s (`ACTIVATE_MS`), or a started capture has not reported for 2 s (`WEDGE_MS`). At most 32
  captures run at once. A capture counts until its thread ends, or until its owner drops it while
  it is stalled; the same application is then refused with `AppAudioError::Busy` until the stuck
  thread returns. While 8 such threads are alive (`MAX_STUCK_CAPTURES`), every capture is refused
  with `AppAudioError::Limit`. On Linux the capture thread runs its own PipeWire loop for at most
  200 ms at a time. The stream's callback copies samples into a buffer of 2 s, past which audio is
  dropped, and the capture thread hands them on in packets of 10 ms; both buffers are allocated
  once. Each packet's position counts the frames handed on and the frames dropped, the packet
  after dropped audio is marked as following lost audio, and its time is `None`. A chunk the
  server marks empty (`SPA_CHUNK_FLAG_EMPTY`) is copied as silence, and a packet made only of such
  chunks is marked silent.
- **Linux list.** While the discovery has a watcher, one more thread keeps the server's registry
  and the state of each playback stream (at most 4,096), and wakes the discovery when a stream
  appears, ends or starts or stops running. A scan reads its streams under a lock and never waits
  for the server. When the server goes away, or has not started, the thread ends, the next scans
  report the error, and a new thread connects at most every 2 s, so a server that starts after
  the source is found. Dropping the watcher closes a pipe that ends the
  thread's loop, and the reaper joins it.
- **Reaper.** Dropping an `AppCapture` or the last `DiscoveryUse` asks the thread to stop and
  hands it to one reaper thread, which joins it once it has ended. The owner never waits, and a
  thread stuck in a system call does not hold up the others.

## Tests

`src/tests.rs` drives the discovery thread and captures with `FakeBackend`: applications that
start, quit and restart with a new process id, peak reads only while the list is visible, an
unchanged list that is not published again, a burst of wakes, a stuck discovery thread, a system
without support, a failed watch, captures that deliver with each packet's position and time,
lose audio, keep delivering while the callback's queue is full, fail later, are refused, find their target
gone, start without audio, hang in activation, stall and end, and the limit of 32 captures. The
tests wait on the discovery's and the capture's own signals.

`src/linux/graph.rs` tests the Linux backend's parts that need no server: reading stream and port
properties, hostile process ids, stream attribution by the server's process id and through the
sound server, names taken from the process table, channel sides, `/proc/<pid>/stat` with parentheses in
the program name, process trees and parent loops, the links a capture plans and their limit, and
the application list with its choice between instances. `src/linux.rs` tests the socket lookup, the
sample buffer, packet positions across dropped audio, streams taking their client's process id, a
target that is gone or whose id another process now holds, and a server that cannot be reached: the list reports it
without the discovery waiting, and a capture fails at once.
