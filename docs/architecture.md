# Architecture

This page describes how StudioCraft's code is organized: its crates and layers, its threads and
how data moves between them, and where new features plug in. It is for contributors.

StudioCraft is a Cargo workspace. The desktop app is the root package (`src/`). The libraries are
in `crates/`, the command-line tool is in `apps/studiocraft-cli`, and the workspace tooling is in
`xtask/`. The command-line tool also runs the Model Context Protocol (MCP) server.

## Layers

A crate may depend only on workspace crates in a lower layer. A dev-dependency may also reach a
crate in the same layer. `cargo xtask layers` checks this against the table in
`xtask/src/layers.rs`.

| Layer | Crate | What it holds |
| --- | --- | --- |
| L0 | `studiocraft-model` | Scenes, sources, scene items, transforms, profiles, settings schemas, and the versioned file format with its migrations. |
| L0 | `studiocraft-media` | The master clock, frame pacing, video frame and audio block types, buffer pools, bounded lock-free queues, mixing and level meters. |
| L0 | `craft-flv` | FLV tags for H.264 and AAC: the file header, `onMetaData`, the AVC and AAC sequence headers, video and audio tags with composition time, the AMF0 values of script data and RTMP commands, and a tag reader. Its tag bodies are also RTMP message bodies. It depends on no StudioCraft crate. |
| L0 | `craft-scopes` | Video scope math: the waveform, parade, histogram and vectorscopes of a borrowed RGBA8 or NV12 frame, their numeric summaries and trace images. It depends on no StudioCraft crate. |
| L0 | `craft-audio-dsp` | Audio DSP shared with the other Craft apps: the `AudioEffect` trait and its effects, the BS.1770 / EBU R128 loudness meter, panning, test signals and resampling. It depends on no StudioCraft crate. |
| L0 | `craft-ease` | Easing curves, keyed tracks, constant-speed Bezier motion paths and damped springs. It depends on no StudioCraft crate. |
| L0 | `craft-gpu-device` | Choosing and opening a wgpu device: the user's adapter choice, corrections for drivers that crash, a software fallback, a capability report, and device health with recovery after a device loss. It depends on no StudioCraft crate. |
| L0 | `craft-gpu-yuv` | RGBA to NV12, P010 and I420 on the GPU with scaling, in BT.601, BT.709 and BT.2020 at limited or full range, and the same conversion on the CPU. It depends on no StudioCraft crate. |
| L0 | `craft-hwenc` | The hardware encoders ffmpeg runs, a one-frame test encode of each with results kept for the session, and an automatic choice ordered by the graphics adapter's vendor with a software fallback. It depends on no StudioCraft crate. |
| L0 | `craft-hwenc-native` | Hardware encoders called through their drivers: NVENC H.264 and HEVC from NV12 or P010 frames on Windows (Direct3D 11) and 64-bit Linux (CUDA), the Windows Media Foundation, macOS VideoToolbox and Linux VA-API hardware H.264 and HEVC encoders from NV12 frames, the Annex B helpers, and the `avcC` and `hvcC` records. Its private `nvenc_ffi`, `mf_ffi`, `vt_ffi` and `vaapi_ffi` modules are the only code that calls the drivers. It depends on no StudioCraft crate. |
| L0 | `craft-app-audio` | Per-application audio: the running applications that have audio, kept current by one discovery thread, a lock-free peak meter for each, and the capture of one application's audio as 48 kHz stereo `f32` with backoff, stall detection and a limit of 32 captures. Windows build 20348 and later capture through WASAPI process loopback; other systems report it as unsupported. It depends on no StudioCraft crate. |
| L1 | `studiocraft-plugin` | The `Source`, `Filter`, `Transition`, `Encoder` and `Output` traits and the `Registry` they are registered in. |
| L1 | `craft-rtmp` | A blocking RTMP and RTMPS publish client on `std::net`: the handshake, chunking, the `connect`, `createStream`, `publish` and `deleteStream` commands, acknowledgements, and a timeout on every step, with the stream key kept out of errors and `Debug` output. It sends `craft-flv` bodies, starts no threads, and is built for native targets only. It depends on no StudioCraft crate. |
| L1 | `craft-transitions` | The Cut, Fade, Fade through color and Swipe transitions: each one's WGSL blend, the values its shader reads, its ease from `craft-ease`, the gains of the old and new audio, and the same blend on the CPU, with typed parameters that carry a version for apps that save them. It depends on no StudioCraft crate. |
| L1 | `craft-packet` | Encoded packets shared by reference through a pool, fanned out to bounded per-subscriber rings with packet and byte caps. A full ring drops packets up to the next keyframe, its receiver skips stale packets to a keyframe, and each subscriber keeps its own counters. When the pool is full, the ring holding the most bytes is emptied, so a stalled destination loses only its own packets. It converts the `craft-video` H.264 encoder's packets and the `craft-hwenc-native` encoders' packets to its packet info. It depends on no StudioCraft crate. |
| L2 | `studiocraft-engine` | Commands with stable ids, undo and redo, source threads, the audio mixer thread and the frame scheduler. |
| L2 | `studiocraft-render` | The wgpu compositor: scenes, filters, transitions, and frame conversion for encoders. |
| L2 | `studiocraft-platform` | Display, window, camera and audio capture, locating ffmpeg, and opening folders and links. |
| L2 | `studiocraft-encode` | The built-in encoders: H.264 video from NV12 frames and AAC audio from the mixer's blocks. |
| L3 | `studiocraft-sources` | The built-in sources and filters. |
| L3 | `studiocraft-audio-tracks` | Audio tracks: the collect and disk threads that write each recorded source and the program mix to WAV files, the take journal, and the repair of takes cut short. |
| L3 | `studiocraft-outputs` | Recording through an ffmpeg process, Matroska recording through the built-in encoders, streaming through the built-in encoders to several RTMP and RTMPS destinations, and the video encoders. |
| L4 | `studiocraft-studio` | The running studio: sources, mixer, compositor, transitions and outputs behind one command interface. |
| L5 | `studiocraft-ui-egui` | The window: menus, preview, docks, properties, filters and settings. |
| L5 | `studiocraft-mcp` | The MCP server: its tools, resources, resource templates, prompts and completions, a studio in the same process without a window (with its file roots and streaming gate), and the control channel client that sends calls to the app. |
| app | `studiocraft` | The desktop binary, its control channel and the screenshot and self-test harness. |
| app | `studiocraft-cli` | `send` for one control request, `mcp` for the MCP server on standard input and output (headless, or `--connect <port>` to the app), `check` for scene collection files and `bench` for the timing run. |
| tool | `xtask` | `cargo xtask` checks. |

These crates have no operating-system or GPU dependency, and `cargo xtask wasm` checks that they
build for `wasm32-unknown-unknown`:

- `studiocraft-model`
- `studiocraft-plugin`
- `craft-flv`
- `craft-scopes`
- `craft-audio-dsp`
- `craft-ease`
- `craft-transitions`

## Threads and data flow

One `MasterClock` (`media::clock`) drives the frame scheduler, the mixer and every timestamp.
Frames and audio blocks come from pools, and the steady-state frame loop does not allocate.

The following table summarizes how each thread receives its work. An empty cell means this page
gives no value for that column.

| Thread | Input queue | Bound | Drop policy | Cost per frame |
| --- | --- | --- | --- | --- |
| Source | None; it publishes video into a `VideoOut` slot and audio into a lock-free queue | The audio queue holds 32 blocks (`plugin::source::AUDIO_QUEUE_BLOCKS`), and at each tick the mixer keeps at most 4 (`engine::mixer::MAX_QUEUED_BLOCKS`) | A full queue drops its oldest block, and the mixer drops the oldest blocks beyond 4 | |
| Audio capture | The device's thread | On Linux, a conversion buffer allocated before the stream starts; a feed keeps at most 32 blocks per callback (`plugin::source::AUDIO_QUEUE_BLOCKS`, about 680 ms) | Blocks past 32 in one callback drop the oldest and are counted | |
| Application audio discovery | System notifications and commands | A command queue of 16 (`craft_app_audio::discovery::COMMANDS`); the list is one `Arc<AppList>` slot, and the meters are 64 atomic rows | A full command queue drops the command; a new list replaces the old one | A scan every 1 s, and at most every 250 ms after notifications; meter reads 30 times a second while a caller asks for them |
| Application audio capture | The system's packets, waited for 200 ms at a time | Buffers sized for the largest packet so far (250 ms to start, 2 s at most); a feed keeps at most 32 blocks per call (`plugin::source::AUDIO_QUEUE_BLOCKS`) | Blocks past 32 in one call drop the oldest and are counted | 3.77 µs to turn a 10 ms packet into blocks, without allocating |
| Screen capture (Linux) | The PipeWire stream | `FrameSlot`: the newest frame and up to 2 spares | A newer frame replaces a frame the source has not taken | One copy of the picture |
| Camera capture | The camera library | `FrameSlot`: the newest frame and up to 2 spares | A newer frame replaces a frame the source has not taken; a frame that cannot be converted goes back to the spares | One conversion of the picture |
| Mixer | One 1024-frame block from every audible source | 4 blocks per source; each output's audio queue holds 256 blocks | The oldest source blocks beyond 4 are dropped | No allocation; the delay lines are allocated with the input list |
| Monitoring | The monitoring mix | 4 blocks, and playback waits for 2 | | No allocation or lock |
| Compositor | The master clock's frame ticks | A ring of 3 GPU textures | | No allocation in the steady state |
| Scopes | The newest program copy, handed over when the thread is idle | One copy | A copy made while the thread is busy is not handed over | |
| Output (ffmpeg) | The program's frames and the mixer's audio blocks | 256 audio blocks, and one second of video frames (the frame rate rounded up, from 8 to 240 frames) | A full video or audio queue drops its oldest item and counts the drop (`media::queue::Fifo`) | |
| Output (built-in encoders) | The program's frames and the mixer's audio blocks; the file thread takes the encode threads' packets | 256 audio blocks, and 64 MiB and 3 s of packets | An encoder that finds the packet queue full waits up to 2 s (`outputs::native::DISK_WAIT`), and then the recording fails. The recording also fails when one encoder falls more than 10 s behind the other (`MAX_LAG_NS`). | |
| Replay buffer | The encode threads' packets | A ring bounded by length and bytes | | |
| Audio tracks collect | Each recorded track's tap of mixer blocks | 32 blocks per tap with its own pool of 34, and a write buffer of 16 to 1024 1 MiB chunks, at least 2 for each recorded track | A full tap reuses its own oldest block; an empty write buffer drops blocks, logs them and writes silence in their place | One 8 KiB copy per track per block |
| Audio tracks disk | The collect thread's chunks | The chunk count of the write buffer | A full or failing disk stops only the audio tracks | Sequential 1 MiB writes |
| Window | `Studio` state | | | |

### Source threads

Each source runs on its own thread (`engine::host`). It publishes video frames into a `VideoOut`
slot and audio blocks into a bounded lock-free queue.

### Audio capture threads

An audio input or output source receives samples on the device's thread: cpal's thread on Windows
and macOS, and on Linux a thread that `platform::audio` starts for each capture to run a PipeWire
stream loop. The Linux stream asks for 48 kHz stereo `f32`, converts each buffer into a buffer
allocated before the stream starts, and stops when its handle is dropped. Listing Linux devices
reads the PipeWire registry and the `default` metadata object and waits at most 2 s for the
server. A failed or ended stream and a lost server connection are reported to the source, which
reopens the device. The rest of the app keeps running.

### Application audio threads

`craft-app-audio` runs one discovery thread while an Application audio capture source exists or
anything else holds a `DiscoveryUse`, and stops it when the last one is dropped. On Windows the
thread lists the audio sessions of every active playback device every second, and at once when a
device is added, removed or changed or a session changes state; scans stay at least 250 ms apart
however many notifications arrive. It reads process names and parents when the set of session
processes changes, and on every scan while a capture target is a parent process, so a parent that
exits is noticed within one scan. A list that differs from the one before is published as an
`Arc<AppList>` with a generation number through an `ArcSwap`, so a reader compares one atomic and
loads the list, without a lock, only when it changed. While `set_visible(true)` is in force, or for
the time given to `read_peaks_for`, the thread reads each application's session meters about 30
times a second into 64 rows of atomics. Each row holds the key's id, the level and the time of the
reading, and readers read it without a lock; a reading older than 250 ms reads as none. Settings and
commands never list applications themselves and never wait for a scan: a source's schema and
`app_audio.list` read the newest list.

When the discovery thread has been inside one system call for more than 5 s, the list reports
`discovery_stalled` and an error. The next source check or `app_audio.list` starts a new thread
beside the stuck one, which ends when its call returns. While two discovery threads are alive, no
third one starts.

Each captured application has its own thread. It waits for the system's packets at most 200 ms at
a time, reports a heartbeat each time, converts each packet into buffers it reuses, and hands the
samples to the source's `AudioFeed`, which queues blocks for the mixer without allocating once the
block pool is warm. The source's thread checks the capture every 250 ms. A capture still asking the
system to activate it counts as stalled after 10 s, and one the system started counts as stalled
after 2 s without a report. The source drops it, and the reaper thread joins its thread once it
ends, so neither the source nor any other capture waits on a capture stuck in a system call. A
dropped stalled capture stops counting against the limit of 32 at once, and the same application
is not captured again until its stuck thread returns. While 8 such stuck threads are alive, no
capture starts. A failed capture opens again after 250 ms,
500 ms, 1 s, 2 s and then every 5 s, and a 33rd capture is refused. Each capture's callback passes a
gate with one compare-and-swap. When the source's application changes, the source closes the old
gate, waits at most 2 ms for a callback inside it to finish its push, and empties the queue, so the
mixer does not play the earlier application's audio. When a callback is still inside after 2 ms,
because the system stopped scheduling its thread, the source opens no new capture until the
callback has left, and then empties the queue again.

A settings form, each check of the list's stamp by an open form, and an `app_audio.list` request
keep the discovery thread running for 60 s without a source.

### Screen capture threads

On Linux a display or window capture (`platform::portal`) starts a thread that asks the desktop's
screen-cast portal for one monitor or window. The desktop shows its own picker while the source
thread keeps running. The portal hands over a PipeWire stream, which a second thread reads.

The stream thread copies each picture, row by row without the row padding, into a spare frame and
publishes it through `platform::screen_frames::FrameSlot`. Two lock-free queues hold the newest
frame and up to 2 spares. A newer picture replaces a picture the source has not taken yet, and
neither thread waits for the other. The stream takes 4-byte RGB formats up to 8192 px a side
and follows a resize with the next buffer.

| Event | What happens |
| --- | --- |
| The picker is canceled, or the desktop closes the session when sharing is stopped from its own controls | The capture clears the picture. The source does not open the picker again until its settings change. |
| The desktop has no portal, the portal returns no stream, the stream fails, or the PipeWire connection is lost | The capture reports it and tries again after 2 s. |
| The capture is dropped while the picker is open | The capture closes the portal session, which ends the request. |
| The capture is dropped, the session closes or the stream ends while the stream runs | The portal thread, which waits for these, closes a pipe the stream loop watches and closes the portal session. |

### Camera capture

A camera source (`sources::camera`) opens a `platform::camera::CameraCapture`, which starts a
thread of its own. The camera handle stays on that thread.

**Opening.** The thread lists the cameras and picks the saved one: the same name and system id,
then the same id unless it is a Linux device node, then the same name. It reads the camera's
formats and opens it through the `nokhwa` crate: Media Foundation on Windows, AVFoundation on macOS
and V4L2 on Linux. On Linux only the V4L2 nodes that can capture video are listed. On Windows every
thread that uses Media Foundation sets up COM (`comedy`). Each capture thread sets up COM for its
stream's life, and each camera list or format read runs on a short-lived thread that sets it up.

**Gray cameras on Linux.** The 8-bit gray (`GREY`) formats are listed and streamed through the
`v4l` crate (`platform::camera_gray`). A camera that sends only gray lists only those formats. A
gray stream waits at most 500 ms for each frame, so its capture thread sees a stop while the camera
sends nothing. For the first frame, and once such a wait runs out, each read waits at most 5 s, and
a read that runs out fails the capture. The source then opens the camera again.

**Converting frames.** Each frame is converted into a spare frame of the same `FrameSlot` the
screen captures use:

| Encoding | Conversion |
| --- | --- |
| YUYV and NV12 | To RGBA with the BT.601 studio-range formulas. |
| Gray and RGB | To RGBA. |
| BGR | To BGRA. |
| MJPEG | Decoded with `zune-jpeg` at the size in its own header. An MJPEG frame without Huffman tables uses the standard ones. |

An uncompressed frame longer than its tight size is read with the row stride V4L2 reports on
Linux, and elsewhere it is read with the stride its length gives. On Windows RGB24 frames arrive
last row first. On macOS each frame's own size and encoding are used, the library's frame queue is emptied
at every frame, and only YUYV formats are offered. The thread copies such frames tight and top row
first into a reused buffer.

**Failures.** A frame that cannot be converted goes back to the spares, and 30 in a row fail the
capture. A panic inside the camera library is caught and reported as an error. A missing or busy
camera, a format the camera does not offer, a failed stream, or no new frame for 5 s (the first
frame included) clears the picture and is reported to the source. The source opens the camera
again after 2 s while the rest of the app keeps running. A camera source whose camera is unplugged
keeps opening it every 2 seconds and resumes when it returns.

**Stopping and sharing a camera.** Dropping the capture sets a stop flag. The thread sees it after
its current frame, stops the stream and ends. A camera is reserved for one capture thread at a
time, so a second camera source showing the same camera reports "the camera is in use by another
camera source". A thread can wait on a camera that sends nothing. Until it ends, a new capture of
that camera fails with "the camera is still closing" and the source tries again.

**Format lists.** The source's settings schema depends on its settings (`Plugin::schema_for`), and
the Format list holds the formats of the chosen camera. On macOS the formats are read from the
device (`nokhwa-bindings-macos`) without changing its active format. A camera that a capture of the
app has open is not opened again to read its formats, and it lists the formats read when the
capture opened it. A camera that another program holds open lists the formats it offered when it
was last read, for up to 32 cameras. The engine checks a choice again when a choice before it
changed, so a format kept from another camera is refused.

**The Properties window.** The Properties window (`ui-egui::forms`) reads every schema on its own
thread and shows "Reading the lists…" until it arrives. While a choice's new lists are read, the
fields after it are locked. The change is sent once they arrive, with a later choice they do not
offer set back to its default. The window reads one schema at a time: a read asked for while one
runs starts when it ends, and the earlier reply is dropped. Closing the window while a change waits
sends the change with the choices after it at their defaults. When the source's settings change
elsewhere while a change waits, the change made elsewhere is kept.

**Plugged-in devices.** Every 3 seconds the window asks the source on another thread for
`Plugin::lists_stamp`, a value that changes when its devices change, and reads the lists again
when the value changed. The camera source hashes its list of cameras, which does not open them.
Each read of the lists also takes the value, just before it reads them. A source that returns
`None` is not asked again.

### Mixer thread

The mixer (`engine::mixer`) pulls one 1024-frame block (48 kHz stereo `f32`) from every audible
source, delays it by the source's sync offset, applies the audio filters, gain, mute and
scene-change fades, and measures peak and RMS levels. It pushes the mix to every output's bounded
queue.

Each input's delay line holds 5 s of audio (the longest offset) and is allocated with the input
list on the sending thread. A new input list takes over the delay lines of the sources it keeps, so
their delayed audio still plays.

The clipped mix also feeds a `craft-audio-dsp` loudness meter (momentary, short-term, integrated,
loudness range and true peak), whose readings the mixer publishes after every block without locks.
Sources set to monitor only are measured but left out of that mix. Every monitored source, after
its delay, filters, gain, mute and fade, is added to a second, preallocated monitoring mix, which
the mixer pushes to the monitoring queue.

### Monitoring thread

While a source is monitored, `studio::monitor` opens the profile's monitoring device through
`platform::audio::AudioSink` (cpal on Windows and macOS). The device's callback pulls the
monitoring mix from a queue of 4 blocks, waits for 2 blocks before it plays, converts 48 kHz to
the device's rate by linear interpolation and maps the two channels onto the device's, without
allocating or locking. A device that fails to open or fails while it plays is reported in a notice
and closed. The mixer and the outputs keep running, and the same device is not opened again until
the setting or the monitored sources change.

### Compositor thread

The scheduler (`engine::scheduler`) wakes on the master clock's frame ticks. The compositor
(`render`) uploads changed source frames, draws the program scene (and the preview scene in studio
mode) into a ring of 3 GPU textures, and converts the program to NV12 (BT.709, limited range) at
the output size for the encoders, through `craft-gpu-yuv`. The window registers the ring's texture
views with egui.

While View › Scopes is open, the compositor also converts the program to a second NV12 copy of at
most 480 × 270 and sends it to the window's scopes queue. Each texel of either copy averages the
part of the program it covers (up to 4 × 4 bilinear taps), and chroma is at half width and half
height. A hard edge in the program therefore shows on the scopes as intermediate levels.

**Graphics devices.** The window opens its device on the adapter Settings › Video › Hardware
acceleration names (`graphics_adapter` in `config.toml`), through `craft-gpu-device`'s `pick`.

| Event | What happens |
| --- | --- |
| The window's device is lost | The app saves everything, notes the loss in `graphics_losses` and starts again. After 3 losses in 10 minutes it starts on a software adapter. |
| A device the app opened without a window (`Gpu::open`) is lost | The device is opened again in place, with no restart. |
| The chosen adapter cannot open the window | The setting becomes Automatic and the app starts again. |
| Automatic cannot open the window | The app starts once more in software for that run (`STUDIOCRAFT_GPU_SOFTWARE` is set for it). |
| A session started with `--no-prefs` or `--control` needs a restart | It does not start itself again, and the status bar asks the user to restart. |
| An uncaptured wgpu error | It counts as a loss. |

When a device is lost, the recording, the stream and the replay buffer stop, the compositor is
closed and nothing is drawn.

### Scopes thread

While View › Scopes is open, the window hands the newest program copy to the scopes thread
(`ui-egui/src/scopes.rs`) when the thread is idle. The thread computes and paints the chosen scope
with `craft-scopes` into buffers it keeps and makes the texture image. The window moves the image
into its texture. The window and the thread share bounded lock-free queues and atomic flags, and
the thread parks while it has nothing to do.

### Output threads

A recording with an ffmpeg encoder runs an ffmpeg process with one writer thread for video and one
for audio (`outputs/src/writer.rs`).

A stream runs the same encode threads as a built-in recording. Its third thread shares each packet
once through `craft-packet` and offers it to every destination without waiting for a network. Each
destination that sends has its own thread, which publishes with `craft-rtmp` from a ring that holds
4 s of the stream (`outputs/src/stream.rs`). A destination that falls behind loses packets up to
the next keyframe, and a destination that fails, stops or reconnects does not affect the others.

A recording with H.264 (built in) runs a video encode thread, an audio encode thread and a file
thread that writes the Matroska file (`outputs/src/native.rs`). The encode threads hand packets to
the file thread through a queue that holds at most 64 MiB and 3 s of media.

In both kinds of output, the video and audio threads share their timeline through one 128-bit
atomic word.

The replay buffer (`outputs/src/replay.rs`) runs the same three threads, and its third thread
keeps the packets in a ring bounded by length and bytes instead of writing a file. A save hands the
ring thread a one-slot queue through a bounded lock-free queue. The ring thread puts a copy of its
packet list, which shares the packet bytes, in that slot. A `save replay` thread per save writes
the clip to its MKV file, and `Studio::tick` reports each finished save.

### Audio tracks threads

While audio tracks run (`studiocraft-audio-tracks`), the mixer stores each recorded source's block
and the program block into that track's `TapSlot` (`media/src/tap.rs`): a bounded lock-free queue
of 32 blocks with its own pool of 34, made on the studio thread and sent to the mixer when the take
starts. Tap sets go to the mixer through a slot of their own that keeps only the latest set, so a
burst of other commands cannot drop the removal of the taps. A take records at most 64 tracks at
once and makes at most 256 in all. A source in the program mix is stored in the same pass that
mixes it (`mix_into_tap`).

A take follows the recording or the stream. The studio starts it just before its output's first
sample and passes that time as the take's frame 0; a take started later gets the time it starts,
and each file's `bext` time reference says where it sits on the output's timeline. A stop passes
the time the output stopped, and every track still recorded ends there.

The `audio-tracks-collect` thread drains the taps into 1 MiB chunks, places each block by its
timestamp from frame 0, cuts audio before frame 0 and after the end, and fills gaps with silence,
counting a gap in a track's audio as lost audio. It does not touch files. The chunk pool (the
write buffer) holds the buffer setting in MiB, and when a track starts the collect thread adds
chunks until there are at least 2 for each track recorded. When the pool is empty because the disk
is slow, the collect thread drops blocks and logs them, and the files still keep time. Each
dropout goes to the disk thread through a queue of 4,096 as soon as it ends.

The `audio-tracks-disk` thread makes the take's folder, appends the chunks to one WAV file per
track, rewrites each header every second, syncs the files that grew and journals them and the
dropouts in the take's `take.journal` every 5 seconds, starts a `<name> part N.wav` file at the
file size limit, and stops the take when free space drops below the reserve or cannot be read. It
checks whether a header rewrite or sync is due after every 16 messages from the collect thread, so a disk slower than the tracks
does not delay them. A panic on either thread stops the take with a write failure.

Status is atomics only. A stop returns at once. The collect thread keeps draining the taps until
the mixer reports that it applied the removal of the taps (up to 5 seconds), so no block stored
before then is lost, and the threads end after the last chunk is written. A change to the recorded
sources during a take waits the same way: the collect thread applies the new track list once the
mixer has applied the matching tap change, so a source that stops being recorded keeps every block
stored for it. At most 2 takes finish their files at once; a new take waits for one of them. When
the app closes it waits up to 10 seconds for takes to finish. While audio tracks are off no tap,
pool or thread exists, and the mixer checks one `Option` per input.

### Window thread

The window reads `Studio` state, runs commands, and shows statistics.

## Extension points

A new source, filter, transition, encoder or output is a type that implements the matching trait
in `studiocraft-plugin`. The type is registered in a `Registry` (see
`studiocraft_sources::register` and `studiocraft_outputs::register`). The window builds its
property forms from each plugin's `SettingsSchema`, so a new plugin does not need window code.

### Video filters

A video filter is one GPU pass (`VideoEffect`). Its `Reframe` sets which part of the input the
pass draws: the whole input, the input with pixels taken off or added at each edge, or one area of
it, up to 8192 pixels a side. The compositor draws each pass into the top-left corner of a pair of
filter textures as large as the largest pass, and the scene item takes the size of the last pass.
`Studio::source_size` applies the same passes, so the window's selection box matches the picture.

A pass reads its input through `input_at`, which returns transparent pixels outside the input, or
`input_pixel`, which reads at a pixel position clamped to the input's edges. A pass can also carry
an `EffectImage`, a picture the compositor uploads once and binds in group 2 for `image_at`. The
compositor releases it when no pass in the plan uses it. A pass with an `Animation` has its
parameters stepped before each frame with the seconds since the first frame that drew it and the
size of its input. The compositor forgets the start time when no pass in the plan uses the
animation.

### Encoders

An encoder names the codec it writes (`Encoder::codec`: H.264, HEVC or AAC). An encoder that runs
inside the ffmpeg program returns its `FfmpegEncoder` form from `Encoder::ffmpeg`, which gives
ffmpeg the codec and its arguments. An encoder without that form is built in: `Encoder::open`
starts its `VideoSession`, which takes NV12 frames and returns packets.

| Output | Encoders it runs |
| --- | --- |
| Recording | Built-in and ffmpeg encoders. With a built-in encoder it writes MKV files only. |
| Replay buffer (plugin id `replay`) | The built-in encoders only. Its `OutputInstance::replay` returns a `ReplayClip` that writes the buffer's packets to a file. |
| Stream | The built-in H.264 encoders only. |

`studiocraft-encode` holds the built-in H.264 encoder (`studiocraft_encode::H264`, plugin id
`h264`) and the AAC encoder (`studiocraft_encode::AacSession`) that built-in recordings use for
sound. `Encoder::availability` reports whether an output can run an encoder, and the output start
check, the Settings encoder lists and Tools › Available Encoders use it.
[Recording](features.md#recording) lists which encoders need ffmpeg.

### Saved plugin versions

Each saved source and filter records its plugin's `kind_version`. When a plugin's version is
higher, `Plugin::migrate` updates the saved settings when the collection loads.

## Commands and undo

Every change to a scene collection is a `Command` (`engine::command`) with a stable id such as
`scene.add` or `item.transform`. Menus, shortcuts, the control channel and the MCP server all run
commands by id. Undoable commands record the collection before the change, and the history keeps
200 steps. Commands with the same coalescing key (for example `item.transform:<id>` during a drag)
within 750 ms of each other make one undo step.

The window adds its own commands, such as `view.stats` or `profile.new`, in
`ui-egui/src/commands.rs`. `craft-keymap` binds shortcuts to all of them.

## Interface

[Interface](features.md#interface) describes the window's theme, menu row, docks, dock layouts,
fonts and title bar. The following table lists the code for each part.

| Part | Code |
| --- | --- |
| Dark and light palettes, compact and comfortable size sets, type scale | `ui-egui/src/kit/tokens.rs` |
| Applying the palettes to egui's dark and light styles at start, the menu bar's style, the focus ring | `ui-egui/src/theme.rs` |
| Segmented control, Record and Go Live buttons with their timers, dock header | `segmented.rs`, `onair.rs` and `panel_header.rs` under `ui-egui/src/kit/`, which depend only on egui, `craft-ui` and `craft-menu` |
| Menu row and output controls | `ui-egui/src/output_bar.rs`, with `craft-ui`'s `fit_toolbar` choosing what moves when the row is narrow |
| Docks | `ui-egui/src/docks.rs`, with `kit/panel_header.rs` for each dock's header |
| Dock strip size | `ui-egui/src/strip.rs`; the strip's top edge and the dividers are `craft-ui` `ResizeHandle`s |
| Dock commands | `dock.move` (Move Left and Move Right), `dock.float` (Undock), `dock.layout_save`, `dock.layout_apply` and `dock.layout_remove` |
| Saved dock sizes, order, visibility, undocked windows and layouts | `config.toml`, with layouts under `dock_layouts` |
| Font stack | `ui-egui/src/fonts.rs`, built with `craft-ui`'s `UiFonts`: Inter Regular for the proportional family, Inter SemiBold for the `semibold` family (dock, section and dialog titles) and JetBrains Mono for the monospace family, all embedded from `assets/fonts`. `StudioApp::install_fonts` installs the stack at start, and the window installs it again on the first frame after the interface language changes. |
| Title bar | `src/titlebar.rs` decides whether the app draws it, from `craft-window`, and `crates/ui-egui/src/title_bar.rs` draws the menu strip as the title bar |

A unit test checks every text pair of both palettes for 4.5:1 contrast and every state mark and
the focus ring for 3:1.

## Saved files

Scene collections and profiles are JSON files with a `format` name and a `version`
(`model::format`). Reading a file at an older version runs the migrations in order. A file from a
newer version is refused with an error. The window keeps scene collections in `scenes/` and
profiles in `profiles/` under the settings folder, and writes them with `craft-atomic`.

## Shared crates

| Need | Crate |
| --- | --- |
| Menu bar model, layout and drawing | `craft-menu` |
| Shortcuts, rebinding and dispatch | `craft-keymap` |
| Settings file with sections, versions and debounced saving | `craft-settings` |
| Open, save and folder dialogs | `craft-dialogs` |
| Atomic file writes | `craft-atomic` |
| Close-request handling | `craft-quit` |
| Icon buttons and combo boxes, the interface font stack | `craft-ui` |
| Embedded interface fonts from `CRAFT_FONTS_DIR` | `craft-fonts-build` |
| Interface translations from `crates/ui-egui/i18n/locales` and `CRAFT_I18N_DIR`, the Language setting | `craft-i18n` |
| Font lookup for text sources | `craft-font-catalog` |
| Picture decoding for image sources | `craft-image-codecs` |
| White balance by temperature and tint for the color correction filter | `craft-color-transform` |
| Frame-rate and tick arithmetic | `craft-time` |
| Control channel framing and session tokens | `craft-control-protocol` |
| JSON-RPC framing for the MCP server | `craft-jsonrpc` |
| MCP sessions and tool results | `craft-mcp` |
| Update checks, the update prompt and finishing an installed update | `craft-update` |
| Who draws the window frame, and the menu strip as the title bar | `craft-window` |

Release file names and package ids are in [docs/releasing.md](releasing.md#file-names).

## Needs without a shared crate yet

- **Native macOS menu bar.** The menu is drawn in the window on every system.
- **Capture on Linux.** Display, window and audio device capture on Linux use the `ashpd` and
  `pipewire` crates directly in `studiocraft-platform`. Windows and macOS use `xcap` and `cpal`.
- **Cameras.** Camera capture on every system uses the `nokhwa` crate directly in
  `studiocraft-platform`, with `v4l` on Linux and `zune-jpeg` for MJPEG frames.
- **Encoding.** StudioCraft runs ffmpeg as a separate process and does not link it.
  [When `ffmpeg` is needed](features.md#when-ffmpeg-is-needed) lists the outputs that need it. The
  built-in encoders come from these crates:

  | Encoder | Crate |
  | --- | --- |
  | H.264 (built in) | The `craft-video` H.264 encoder and the `craft-audio` AAC encoder |
  | NVIDIA NVENC H.264 or HEVC (built in) | `craft-hwenc-native`, which loads the NVIDIA driver at run time |
  | Windows Media Foundation H.264 or HEVC (built in) | `craft-hwenc-native`, which loads `mfplat.dll` at run time |
  | Apple VideoToolbox H.264 or HEVC (built in) | `craft-hwenc-native` on macOS, which calls the VideoToolbox framework |
  | VA-API H.264 or HEVC (built in) | `craft-hwenc-native` on 64-bit Linux, which loads `libva.so.2` and `libva-drm.so.2` at run time |

  The VA-API encoder runs on the recording's encoder thread, as the other built-in encoders do. It
  keeps four frames in flight, so the thread waits for the GPU only when all four surfaces are
  taken, and then for the frame submitted three frames before. Frames reach it as NV12 in memory
  and are copied into a surface, so a frame rendered on the GPU is read back to the CPU and
  uploaded again. VA-API can import a DMA-BUF as a surface; the encoder does not do this yet, and
  the copy costs 80 µs at 1080p and 1.2 ms at 4K on the encoder thread. The VA-API test that
  lists the encoder in Settings initializes the driver of every render node on the calling
  thread. When every node's driver lists no encoder for the codec, that answer stands until
  StudioCraft restarts and later tests do not initialize the drivers again.

  `craft-flv` writes and reads the FLV tags and AMF0 values that RTMP carries, `craft-packet`
  fans encoded packets out to several destinations, and `craft-rtmp` publishes a stream to an RTMP
  or RTMPS server. No output uses them yet.
- **Media files.** The media file source plays an Ogg file that holds one Vorbis stream at 8 to
  192 kHz and nothing else on a thread of its own, with the `craft-media` Ogg reader and the
  `craft-audio` Vorbis decoder. The thread holds the file's packet index and one packet at a time,
  and waits for each packet's time before publishing it. Every other file, chained Ogg files and
  Vorbis streams at other rates included, plays through two ffmpeg processes.

## Dependency licenses

`cargo xtask licenses` accepts permissive software licenses (MIT, Apache-2.0, BSD, Zlib, ISC and
similar), the OFL-1.1 and Ubuntu font licenses of egui's built-in fonts, and the IJG license.
Two packages are accepted by name: `option-ext` (MPL-2.0), which `dirs-sys` uses through
`craft-settings`, and `libfuzzer-sys` ((MIT OR Apache-2.0) AND NCSA), which `Cargo.lock` lists
for `rav1e`. `rav1e` is an optional dependency of `craft-video` that no StudioCraft crate enables,
so neither package is built. The list is `EXCEPTIONS` in `xtask/src/licenses.rs`.
