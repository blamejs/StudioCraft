# craft-app-audio

Per-application audio for a Craft app's mixer: the list of running applications that have audio,
a peak meter for each one without capturing it, and the capture of one application's audio as
48 kHz stereo `f32` samples. The crate depends on no app crate. It builds on every system and
reports capture as unsupported, with a reason, where the system has no way to do it.

| Item | What |
|---|---|
| `Discovery` | the list of applications, kept current by one thread while anything holds a `DiscoveryUse`, and the captures; `Discovery::system()` is the process's one discovery on the system's backend |
| `AppList`, `AppInfo` | one scan's applications: key, name, program path, process ids, the process a capture targets, whether it plays sound, its peak row |
| `AppKey` | an application named so the name survives a restart with a new process id: the program file's name in lower case, with its path in a list only to tell apart two applications whose files have the same name; `AppInfo::choice` saves the name with the path, and `AppList::find` takes the application at that path, or else the only one with that file name, and never guesses between two |
| `PeakTable` | 64 rows of atomics with each listed application's peak level, read without a lock |
| `AppCapture` | one capture: its state, its error, a stall check, and its counters; dropping it stops the thread without waiting for it |
| `Backoff` | the waits before opening a failed capture again: 250 ms, 500 ms, 1 s, 2 s, then 5 s |
| `Backend`, `Watcher` | the system's side: support, listing and meters, and the capture loop |
| `fake::FakeBackend` | a scripted backend for tests: applications start and quit, captures deliver, fail, stall or end |

## Systems

| System | Listing | Meters | Capture |
|---|---|---|---|
| Windows build 20348 and later (Windows 11, Windows Server 2022) | The audio sessions of every active playback endpoint ([audio sessions](https://learn.microsoft.com/en-us/windows/win32/coreaudio/audio-sessions)) | `IAudioMeterInformation` peak values | Process loopback with the process's child processes ([`AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS`](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params), [`ActivateAudioInterfaceAsync`](https://learn.microsoft.com/en-us/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-activateaudiointerfaceasync)) |
| Earlier Windows builds | Unsupported | Unsupported | Unsupported |
| macOS and Linux | Unsupported | Unsupported | Unsupported |

On Windows the key is the program file's name. A capture targets the topmost process in the
parent chain whose file has the same name, and includes that process's child processes, so an
application that plays from several processes is captured whole. When separate instances of one
program run, each with its own process tree, the list shows them as one application and a
capture takes one tree: a playing instance before a silent one, then the lowest process id. A
source captures again when that choice changes. Two instances that play at once are not captured
together. Sessions shared by several processes and the system sounds session are not listed.

## Threads

- **Discovery.** One thread per `Discovery`, started by the first `acquire` and stopped when the
  last `DiscoveryUse` is dropped. It lists the applications every 1 s, and at once when an
  endpoint is added, removed or changed or a session changes state. Each list that differs from
  the one before is published as an `Arc<AppList>` with a generation number through an `ArcSwap`:
  readers compare `generation()`, an atomic, and load the list without a lock when it changed.
  While `set_visible(true)` is in force, and for the time given to each `read_peaks_for` call, it
  reads every application's meter about 30 times a second into the `PeakTable`. It reads the
  meters without holding a lock, and an application whose meter gives no reading shows 0. Commands reach it through a queue of 16; a full queue drops the
  command.
- **Capture.** One thread per `AppCapture`. It waits for the system's packets at most 200 ms at a
  time and reports a heartbeat each time, converts each packet into a buffer it reuses, and hands
  the samples to the caller's callback. After the first packets it does not allocate unless a
  packet is larger than any before. `stalled(now)` is true when a running capture has not
  reported for 2 s. At most 32 captures run at once; a capture counts until its thread ends.
- **Reaper.** Dropping an `AppCapture` or the last `DiscoveryUse` asks the thread to stop and
  hands it to one reaper thread, which joins it once it has ended. The owner never waits, and a
  thread stuck in a system call does not hold up the others.

## Tests

`src/tests.rs` drives the discovery thread and captures with `FakeBackend`: applications that
start, quit and restart with a new process id, peak reads only while the list is visible, an
unchanged list that is not published again, a system without support, a failed watch, captures
that deliver, fail, stall and end, and the limit of 32 captures. The tests wait on the
discovery's and the capture's own signals.
