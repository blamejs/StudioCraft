# Features

This page describes everything StudioCraft does, one section per area. It is for people who use
the app and want to know what a menu item or a setting does. The tuned values, such as timings,
sizes and bitrates, are listed in [MECHANICS.md](../MECHANICS.md).

## Sources

A scene is a stack of sources. You add, rename, duplicate, reorder and remove scenes and sources.
In the preview you drag, resize and rotate sources. A source that is not rotated snaps when an
edge or its center line comes within 8 points of a canvas edge, a canvas center line, an edge of
the action safe area (3.5% in from each canvas edge) or the title safe area (5% in), or an edge or
center line of another visible source, and lines show what it snapped to. A move snaps on each
axis, and each edge or center line of the source that then lies on a line shows that line. A corner drag snaps the edges it moves and keeps the aspect ratio: the edge closer to its
line snaps and the other edge follows the ratio. With Shift held the corner drag changes the ratio
and each edge snaps on its own. Holding Alt (Option on macOS) turns snapping off. One drag is one
undo step. You can lock a source so it can't be moved, or lock the whole preview. A selected locked
source shows a dashed outline and no handles. The snap lines and the locked outline take their
colors from the window's light or dark mode. Each scene item can be
shown or hidden, and every change can be undone.

The Sources dock lists the scene's items, the top item first. Each row shows an icon for the
source's type before its name; a color source's icon shows its color. The eye and lock buttons at
the end of the row show or hide the item and lock or unlock it, and a hidden item's name is drawn
muted. A source that cannot draw shows a warning icon, and its tooltip gives the reason. A window
capture whose window cannot be captured shows the warning icon in place of its type icon and reads
"No window". Under the scene's items, a "Heard in every scene" group lists the global audio sources
(see [Audio](#audio)), which include the applications routed from the Application Audio dock. The
group takes at most half the dock's height. A double-click on one of its rows opens the source's
Properties, and a right-click opens a menu with Properties…, Filters… and Rename.

The Transform menu (in Edit and in a source's context menu) has Fit to Screen, which shows the
whole source in the canvas at its aspect ratio; Fill Screen, which covers the canvas at the
source's aspect ratio and leaves the parts past the canvas edges out of the picture; and Stretch to
Screen, which makes the source the canvas's size. When the canvas size changes, every scene is laid
out for the new size in one undo step: a source that was fit, filled or stretched with these
commands is fit, filled or stretched again, and any other source keeps its place in proportion to
the canvas ([`canvas.relayout`](control-protocol.md#canvasrelayout)). Moving, resizing, rotating
or cropping a source ends its fit, fill or stretch.

### Built-in sources

| Source | What it captures | Systems |
| --- | --- | --- |
| Color | A solid color. | All |
| Image | A picture file. | All |
| Text | Text in a chosen font. | All |
| Media file | A media file. | All |
| Display capture | A whole display. | Windows, macOS and Linux |
| Window capture | One window. | Windows, macOS and Linux |
| Staged windows | One of up to six windows or displays picked ahead of time, swapped from its Properties, a hotkey or a command ([Staged windows](#staged-windows)). | Windows and macOS |
| Camera | A webcam or capture card. | Windows (Media Foundation), macOS (AVFoundation) and Linux (V4L2) |
| Audio input capture | A microphone or other input device. | Windows, macOS and Linux |
| Audio output capture | The sound the computer plays (desktop audio). | Windows and Linux |
| Application audio capture | The sound one application plays. | Windows 11, Windows Server 2022 and Linux (PipeWire) |
| Live Captions | The words heard in the program audio, while live captions run. | All |

### Display and window capture on Linux

On Linux, display and window capture go through the desktop's screen-cast portal
(`xdg-desktop-portal` with a backend for your desktop) and PipeWire. The desktop's own picker
asks for the display or window when the source starts. After the picker is canceled or sharing is
stopped, changing any of the source's properties opens the picker again. The list of displays
for the canvas size (Settings › Video › Match display and `video.displays`) does not use the
picker: it comes from the Wayland compositor's outputs, or from RandR under X11.

### Staged windows

A Staged windows source holds up to six windows or displays, chosen in its Properties as Window 1
to Window 6, and shows one of them: the window or display in the slot that Presented window
names. Each slot's list offers the open windows and then the connected displays. Every staged
window or display keeps its capture open. The presented one is captured at the capture rate and
each of the others once a second, so a swap shows the new window or display on the next frame
without opening its capture again. A slot keeps its window after the window closes. Every 2
seconds the source looks for it again: first a window with the same program and title, then one
with the same title, then another window of the same program. A slot also keeps its display while
the display is disconnected, and every 2 seconds the source looks for a display with the same id;
until one is connected, the source reports that the display is not connected. A slot that holds a
display saves the character U+001E followed by the display's id, the id a Display capture source
saves for that display. Any other value is a window, so collections saved before displays could
be staged open with the same windows.

To swap the presented window or display, change Presented window in the Properties, or run
`present.window` (a slot), `present.next` or `present.previous`
([`present.*`](control-protocol.md#presentwindow-presentnext-and-presentprevious)). Next and
previous skip empty slots and wrap around. Settings › Hotkeys lists Present Next Staged Window,
Present Previous Staged Window and Present Staged Window 1 to 6, with no shortcut until you set
one. Without a source named, these commands change every Staged windows source in the scene
collection that has a window or display in the slot they move to. The presented slot is saved
with the collection. Swapping the presented slot, from the Properties or otherwise, is not an undo
step, and Edit › Undo does not change it.

While the presented window or display cannot be captured (the window closed or is minimized, the
display is disconnected, or the slot is empty), the source shows the placeholder that "While the
presented window cannot be shown" selects: a color, black by default, at the size of the last
picture (1920 x 1080 before the first one), or the last picture of that window or display. After
a swap, the source never shows the picture of the window or display presented before. The source
is not offered on Linux, where the desktop's picker chooses each captured window and display.

### Audio capture on Linux

On Linux, audio input and output capture go through the PipeWire sound server. The device lists
show PipeWire's devices by their descriptions, with the default device first.

### Application audio capture

An Application audio capture source records what one application plays, and nothing else the
computer plays. Its Properties list the running applications that have an audio session on any
active playback device, sorted by name. The capture also records the processes the application
starts. The source names the application by its program file and folder, so it keeps capturing
after the application quits and starts again, and after an update moves the program when no other
program with that file name runs. Applications whose program files share a name are listed with
their folders. While the application is not running, the source is silent. When a capture fails,
the source opens it again after 250 ms, 500 ms, 1 s, 2 s and then every 5 s, and the source shows an
error in the window; the system's own text goes to the log. When the system refuses the capture,
the source waits 5 s at once. A saved choice whose value is not an application value loads as a
silent source with an error. At most 32 applications are captured at once. When several separate
copies of one program run, the list shows one entry for them and the source captures one copy: the
copy captured last time while it makes sound or no other copy does, otherwise the copy that made
sound in the last 3 seconds, then one whose audio stream is running. Two copies playing at once are
not captured together. An application started by another listed application, such as a helper
program, is already recorded by that application's capture.

Capturing one application uses Windows process loopback capture
([`AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS`](https://learn.microsoft.com/en-us/windows/win32/api/audioclientactivationparams/ns-audioclientactivationparams-audioclient_process_loopback_params)),
which needs Windows build 20348 or later: Windows 11 or Windows Server 2022.

On Linux the list holds the applications that have a playback stream on the PipeWire sound server,
named by the program file of the process that owns the stream. A stream that the sound server
passes on for an application using the PulseAudio interface names its own process, and nothing
checks that name. StudioCraft counts such a stream for the process it names only while that
process has an open connection to the PulseAudio socket. When the process has none, or when
StudioCraft cannot read the process's open files or the system's socket list, the stream is not
listed or captured. The capture links each of
the application's playback streams, and those of the processes it starts, to a recording stream of
StudioCraft's own, and links streams that start later as they appear. The application still plays
on its own device. Linux lists no meter levels, so when several copies run the source keeps the
copy captured last time while its stream runs or no other copy's does, otherwise it takes a copy
whose stream is running, then the one with the lowest process id.

On earlier Windows builds, macOS and Linux without the PipeWire server the source lists no
applications and says why, and Audio output capture still records everything the computer plays.
The `app_audio.list` and `app_audio.status` commands report the same lists and support
([Control and automation](#control-and-automation)).

### Camera

The camera source's Properties window lists the cameras and the chosen camera's formats. A format
is a picture size, a frame rate and a pixel encoding: MJPEG, YUYV, NV12, gray, RGB or BGR. On
macOS only YUYV formats are offered. The window reads these lists in the background and shows
"Reading the lists…" until they arrive.

The camera source behaves as follows:

| Situation | What happens |
| --- | --- |
| Format is Default | The camera opens at its largest picture up to 1920 × 1080 at 30 fps or more. |
| A camera is plugged in or out while the Properties window is open | The lists update within about 3 seconds. |
| A saved camera is not plugged in | The camera shows as "(not connected)". |
| The camera is missing, in use by another program, unplugged, or sends nothing for 5 seconds | The Properties window reports it, and the source opens the camera again every 2 seconds. |
| Several camera sources show the same camera | Every source shows the camera's pictures, and the camera is opened once. It stays in the format of the source that opened it first. A source that chose another format shows the picture in the format the camera is already open in, and its Properties window notes that the camera is already open in another format. The go-live check does not count that note as a problem. The camera closes when the last of these sources is removed. |
| A camera is listed for the first time on macOS | The system asks for camera access. |

[docs/architecture.md](architecture.md#camera-capture) describes how the capture thread opens,
reads and reopens a camera.

## Filters

Each source can carry a list of filters. The video filters are color correction, chroma key,
color key, crop and pad, image mask, scroll and sharpen. The audio filters are gain, noise gate,
compressor, limiter and a three-band equalizer.

| Filter | What it does |
| --- | --- |
| Color correction | Sets gamma, contrast, brightness, saturation and opacity, and the white balance by temperature and tint. |
| Chroma key | Makes the pixels near a key color transparent by their chroma distance from it, with key color spill reduction. |
| Color key | Makes the pixels near a key color transparent by their RGB distance from it, with Similarity and Smoothness, and adjusts the rest by gamma, contrast, brightness and opacity. |
| Crop and pad | Takes pixels off each edge of a source (a positive value), adds transparent pixels (a negative value), or shows one area of it. The source takes the new size in every scene. |
| Image mask | Uses a picture file as an alpha mask (from its brightness or its alpha) or blends it with the source (multiply, add or subtract). The picture is stretched over the source or centered at its own size, and the filter then applies a color and an opacity. |
| Scroll | Moves a source's picture at a horizontal and a vertical speed in pixels per second. The picture repeats (Loop) or moves out once, in an area whose width and height can be limited. |
| Sharpen | Makes edges crisper by a Sharpness from 0 to 1. |
| Gain, noise gate, compressor, limiter and equalizer | Change the level and tone of a source's audio. The noise gate, compressor and limiter report how far they turn the audio down through [`filter.meter`](control-protocol.md#filtermeter). |

[MECHANICS.md](../MECHANICS.md#video) lists each filter's default values and limits.

## Transitions

The transitions are cut, fade, fade through a color, and swipe, with a duration from 0 to 20
seconds. Fade, fade through a color and swipe have an Easing setting: Linear, Ease in, Ease out,
or Ease in and out.

### Studio mode

In studio mode you edit a preview scene beside the program scene. You then send the preview to
the program with the chosen transition, or with a quick cut or fade.

## Audio

A new scene collection has two global audio sources that every scene records: Desktop audio, the
system's default output device, and Microphone, the default input device. Desktop audio captures
sound on Windows and Linux only.

The Audio Mixer dock shows peak and RMS meters with peak hold, a volume fader in dB, and a mute
button for every audible source. A muted source's mute button shows the muted speaker on a
warning-colored fill. The settings button at the end of each channel opens a menu with Properties…,
Filters… and Rename. Rename asks for the new name in a prompt. The fader runs from silence to +20 dB of boost (StudioCraft's own
limit), with a mark at 0 dB. A boosted source is louder than its input, and the final mix is still
clipped at full scale. The `source.volume` command takes a linear `gain` from 0 to 10, where 1 is
0 dB and 10 is +20 dB; a value outside that range, or one that is not a number, is an error and
changes nothing. The mixer also measures the final mix's loudness (momentary,
short-term, integrated, loudness range and true peak). Each profile keeps a loudness target band,
−18 to −14 LUFS by default (StudioCraft's own value). The `audio.loudness` command and the MCP
tool `audio_loudness` report the readings with the band. The window does not show these readings
yet.

A source's Properties window sets two more audio options:

- **Sync offset** delays the source's audio by 0 to 5000 ms to line it up with its picture.
- **Monitoring** is off, monitor only, or monitor and output. With monitor only, you hear the
  source, and recordings and streams leave it out.

Monitored sources play on the device chosen in Settings › Audio. Monitoring works on Windows and
macOS.

### Application Audio dock

Docks › Application Audio shows a dock that lists a lane for each audio input source, then a lane
for each running application that has audio. Each lane has a live meter. The meter of a lane the
program does not hear is drawn gray, and so is the name of a lane without a check box. An
application lane's check box is labeled with the lane's name, and its tooltip says that the
application is not running when it has quit. The meter of a lane with a source shows the source's level in the mix,
and is silent while the mix has no level for it. The meter of an application lane without a
source shows the level the system reports for the application, without capturing it. The program hears a lane when its source is a global
audio source or is shown in the program scene, unless the source's monitoring is set to monitor
only.

The check box of an application lane routes the application into the program in every scene,
together with the microphone. Desktop audio also records what a routed application plays on the
device it captures. While the program hears Desktop audio unmuted and a running application is
routed and not muted, the dock warns that the program can hear that application twice. Routing adds an Application audio capture source for the application
to the global audio sources, or uses a source of the collection that already captures it, and
sets that source's monitoring to monitor and output when it was monitor only. Clearing the check
box takes that source out of the global audio sources, and Edit › Undo puts it back. A source that
the program scene shows is still heard after its check box is cleared. The check box is
checked only while a routed source reaches the program. A routed source set to monitor only leaves
it clear, and checking it routes the application again.

When one application runs inside another one's process tree, such as a helper program, the capture
of the outer one already records it, and the dock does not route one of them while the program
hears the other. A routed application that quits keeps its lane. When two listed programs have the
same file name, their lanes show the folder as well. When the program hears two sources of one
application, the second source gets a lane of its own, named after the source, with its own volume
slider and mute button and no check box.

A lane with a source has a volume slider in dB, from silence to +20 dB with a mark at 0 dB, the same
as the Audio Mixer's fader, and a mute button. Its settings menu opens the
source's Properties… (sync offset and monitoring) and Filters…. The dock is hidden in a new
layout. On a system that cannot capture one application's audio, the dock lists the microphones
and says why no applications are listed.

## Recording

Recordings are MKV, MP4 or MOV files at four quality levels: Compact, High, Pristine and Lossless.
The Lossless level encodes the video frames without compression loss and writes very large files.
The software encoder runs x264 with `-qp 0`, and NVENC through `ffmpeg` runs with
`-tune lossless`. A Lossless recording also stores its audio without loss, as FLAC in an MKV file
and as ALAC in an MP4 or MOV file. The other levels encode the audio as AAC.

StudioCraft writes recordings to the folder set in Settings › Output. When that setting is empty,
it uses your Videos folder, then your home folder when the system has no Videos folder, and then
the folder StudioCraft was started in. It names each file `YYYY-MM-DD HH-MM-SS.<ext>`. File › Show
Recordings opens the folder.

### Encoders

x264 is the default encoder for recordings, and H.264 (built in) for streams. Settings › Output and
Settings › Stream show the chosen encoder's settings, such as the x264 and NVENC presets.

| Encoder | Records | Needs `ffmpeg` | Systems |
| --- | --- | --- | --- |
| Software (x264) | MKV, MP4 and MOV at every level | Yes | All |
| H.264 (built in) | MKV at Compact, High and Pristine, with a Speed, Balanced or Quality preset, and streams | No | All |
| Hardware (set in Settings › Video) | MKV, MP4 and MOV, and streams, with the hardware encoder set in Settings › Video › Hardware acceleration | Yes, except for the built-in encoders below and for streams | All |
| NVIDIA NVENC H.264 and HEVC (built in) | MKV, through the NVIDIA driver | No | Windows and 64-bit Linux |
| Windows Media Foundation H.264 and HEVC (built in) | MKV, with the hardware encoders Media Foundation lists | No | Windows |
| Apple VideoToolbox H.264 and HEVC (built in) | MKV, with the VideoToolbox hardware encoders | No | macOS |
| VA-API H.264 and HEVC (built in) | MKV, with the VA-API hardware encoders through libva | No | 64-bit Linux |

H.264 (built in) uses StudioCraft's own H.264 and AAC encoders. The Hardware encoder uses the
built-in hardware encoders for MKV recordings, and the built-in hardware H.264 encoders for streams,
with H.264 (built in) when none works. [Hardware acceleration](#hardware-acceleration)
describes how it picks one.

### When `ffmpeg` is needed

MP4 and MOV recordings, recordings at the Lossless quality level, and recordings with x264 or a
hardware encoder outside the built-in ones run in a separately installed `ffmpeg`. Streams do not
need it.
A Media file source plays Ogg Vorbis files at 8 to 192 kHz without `ffmpeg`, and plays every other file through `ffmpeg`. StudioCraft looks for `ffmpeg` on the `PATH`, or at the location set in Settings › Output.
Recording uses x264 until H.264 (built in) is chosen in Settings › Output. No release package
includes `ffmpeg`. The `.deb` and `.rpm` packages list it as a recommended package. The Flatpak
runtime has no `ffmpeg`, and the Flatpak sandbox cannot run an `ffmpeg` installed on the system.

Tools › Available Encoders lists the built-in encoders and the encoders the installed `ffmpeg`
offers, and says when `ffmpeg` was not found.

### Recording markers

File › Add Recording Marker (`record.marker`, which can be given a shortcut in Settings › Hotkeys)
marks the running recording at that moment. A marker is named "Marker N" for the recording's Nth
marker, and a recording holds at most 10,000 markers. When a recording with at least one marker
stops, or the app closes during it, StudioCraft finishes the recording file and then writes
`<recording name>.otio` beside it: an
[OpenTimelineIO](https://opentimelineio.readthedocs.io) timeline with the recording as one clip on
a video track and on an audio track, and each marker on the timeline at the frame that holds its
time. A marker's time leaves out the time the recording skipped while the computer was suspended.
The clip is as long as the frames written to the recording file, so a recording whose encoder fell
behind before the stop gets a shorter clip, and a marker past the clip's end goes on its last
frame. The timeline names the recording by its file name, so the two files can be moved together.
File › Import in FilmCraft opens the timeline as a sequence with the markers on it. A file of that
name that already exists is not replaced: the timeline goes to `<recording name> (2).otio`, or the
next free number up to 999, and the status bar names the file. A recording without markers gets no
timeline file, and neither does a recording whose file could not be finished; the status bar says
the markers could not be saved.

The `marker.add` command marks the running recording and the running take of
[audio tracks](#audio-tracks) at the same moment with one marker. Both outputs get the same name,
"Marker N" for the session's Nth marker when none is given. A session lasts from the start of the
first of the two outputs until both have stopped, and holds at most 10,000 markers (StudioCraft's
own limit). When one output does not keep a marker, for example because 1,024 markers are waiting
to be written to the take, the reply names the cause and the other output keeps it. `marker.list`
lists the session's markers, and after the session ends it lists that session's markers until the
next session starts ([`marker.add` and `marker.list`](control-protocol.md#markeradd-and-markerlist)).

The `marker.rename` command renames one of the session's markers in the recording and the take of
audio tracks, while they run and after they have ended. While an output runs, the new name goes
into the file it writes at its end: the recording's timeline, and the take's session file, which
repair after a crash writes with the new name too. After an output has ended, its timeline or
session file is written again with the new name, through a temporary file and a rename. When one output cannot take the new name, for example because its file
was changed in another app, the reply names the cause and the other output keeps the new name
([`marker.rename`](control-protocol.md#markerrename)).

### Replay buffer

File › Replay Buffer keeps the last seconds of the program in memory with the built-in H.264 and
AAC encoders. File › Save Replay writes them to an MKV file named `Replay <date> <time>.mkv` in
the recording folder, and the buffer keeps running. Settings › Output sets the length (5 to 3600
seconds, 20 by default) and the memory limit (32 to 16384 MB, 512 by default). Start or Stop
Replay Buffer and Save Replay can be given shortcuts in Settings › Hotkeys. While the buffer runs,
the status bar shows REPLAY with the time it has run.

### Audio tracks

Settings › Output › Audio tracks writes each audio source and the program mix to their own 32-bit
float 48 kHz stereo WAV files while a recording, the stream or either runs. The files go to
`<recording name> Audio/Audio Files/` beside the recording, or
`Stream YYYY-MM-DD HH-MM-SS Audio/Audio Files/` for a stream alone: `00 Program.wav` and one file
per source, named after the source. When the folder name is taken, the take uses
`<name> (2) Audio`, `<name> (3) Audio` and so on. A source's Properties window has a "Record as its
own track" check. Until it is set, a source whose monitoring setting sends it to the program mix is
recorded.

A take follows one output. With Either, it follows the recording while one runs and the stream
otherwise: a recording that starts or stops during a stream ends the take, and the next take starts
in its own folder. When the recording then fails to start, its folder is removed and the stream's
next take starts from the moment the earlier one ended. A take that starts with its output starts at the output's first sample and ends
at the time the output stopped. Every file of a take starts at the same moment, including the file
of a source added during the take, and keeps time through gaps and a slow disk, which are filled
with silence. Every file ends at the take's end, except the file of a source removed for more than
60 seconds, which ends where the source left. Each file carries
a broadcast WAV `bext` chunk (EBU Tech 3285): its time reference is the sample, at 48 kHz, of the
recording's or stream's timeline that the file's first sample is at. It is 0 for a take that
started with its recording, and the take's start for a take that started later, such as after the
setting was turned on during a recording.

A file that reaches the file size limit (1 GiB by default, 1 to 3 GiB) continues in
`<name> part N.wav`, whose time reference is where that part starts. An editor that limits the
length of an imported file may need the 1 GiB setting for long takes. A full or failing disk stops
only the audio tracks, and a notice gives the reason. The take's files keep every whole frame
that reached the disk and are finished at once; a failure while the take was finishing its files
leaves it for repair. A take cut short by a crash is repaired
at the next start, or with `studiocraft-cli audio-tracks repair <folder>`. Each repaired file keeps
the audio that reached the disk, so the files of a repaired take can end at different points.

A take that makes its own `<name> Audio` folder also writes `<name>.scraft` there, a session file
for the audio editor. The file appears with no tracks when the take starts and is replaced with the
full session when the take ends, stops early or is repaired. The session holds one track for each
of the take's tracks, the program mix first. Each file is one clip at the sample of the output's timeline where the
file starts, and each part follows the one before it. Each dropout (audio lost before the disk) is
a selection on its track's marker lane. File › Add Audio Tracks Marker (`audio_tracks.marker`, which
can be given a shortcut in Settings › Hotkeys) marks the running take at that moment, and the
session lists each such marker at its sample. A session lists at most 10,000 markers and dropouts,
markers first.
Its comments say when the take stopped early and how many markers and dropouts it left out. When the
full session cannot be written, for example on a full disk, the take stops with that error and is
left unfinished, and a repair of the take writes the session.

Settings › Output › Audio tracks › After a take sets what happens when a take's files are complete:
Nothing (the default), Show the folder, or Open in the audio editor. Audio editor sets the editor's
program; when it is empty, `soundcraft` on the `PATH` is used. When the editor cannot be found or
started, the take's folder opens instead and a notice gives the reason. A take that ends
because the next one starts (a recording that starts or stops during a stream) opens nothing, and so
does a take whose files are complete only after a newer take has opened. Takes that the app finds
complete at the same time open in the order they ended. A take that ends because the app closes
is opened as the app closes, and the app waits up to 3 seconds for the editor or the folder to open.
File › Open Last Audio Tracks in Editor opens the newest take's session again, or the session of a
repair started after that take stopped, and can be given a shortcut in Settings › Hotkeys.

### Live captions

Tools › Live Captions turns the program audio into text on this computer with a Whisper speech
model. A Live Captions source shows the text on the program: by default the last 2 lines of up to
42 characters in white on a translucent black box. A line is cleared 4 seconds after it last changed. Its
Properties window sets the speech model and language that Tools › Live Captions starts with, and
the lines, characters per line, font, size, colors, outline and alignment. The phrase being spoken
appears at once and is corrected when it is finished.

| Model | Download | Notes |
| --- | --- | --- |
| Whisper tiny | 146 MiB | Fastest, least accurate |
| Whisper base | 280 MiB | The default |
| Whisper small | 925 MiB | Most accurate, and slowest |

A model is downloaded once, with Tools › Download Speech Model (the model the first Live Captions
source names) or `captions.download`, into `StudioCraft/models` in the user's local
data folder (`%LOCALAPPDATA%` on Windows, `~/Library/Application Support` on macOS,
`~/.local/share` on Linux). Each file is checked against its size and SHA-256 sum. Recognition runs
on the processor, and no audio leaves the computer.

A recording that ran while captions ran gets caption files beside it: `<recording name>.srt`,
`.vtt` and a plain-text `.txt`, timed from the recording's start. A recording in which no phrase
was recognized gets no caption files. A file of that name that already
exists is not replaced. Start or Stop Live Captions can be given a shortcut in Settings › Hotkeys.

## Streaming

StudioCraft streams to Real-Time Messaging Protocol (RTMP and RTMPS) servers at a constant
bitrate. Set the server and the stream key in Settings › Stream, then click Go Live. Streams run
in StudioCraft with H.264 (built in) or a built-in hardware H.264 encoder, without `ffmpeg`.
Settings › Stream also sets the video bitrate, the audio bitrate, the video encoder and the
keyframe interval. The defaults are listed in
[MECHANICS.md](../MECHANICS.md#recording-and-streaming).

A profile holds up to 16 stream destinations, each with its own server, key and on or off setting.
Settings › Stream edits the first one. A stream sends to every destination that is on, and each
destination connects, reconnects and stops on its own: one that fails does not stop the others.
A lost connection, or one that cannot be made, is tried again as the Automatic reconnect section of
Settings › Stream says. By default the waits are 2, 4, 8 and 16 s and then 30 s each, and a
destination gives up after 10 failed tries in a row. The first wait can be 1 to 60 s, the longest
wait from the first wait to 600 s, and the number of tries 1 to 100; each wait doubles the one
before it up to the longest wait. StudioCraft cuts each wait by a random 0 to 20 %, never to below
1 s, so destinations that lost their connections together do not all try again at once. With
Automatic reconnect off, a destination stops at its first failure. A destination gives up at most
the sum of its waits (at most the number of tries times the longest wait, or 5 s for a wait after a
refused stream key when that is longer) after its first failure, plus up to 10 s per try for the
name lookup and up to 55 s per try for the connection (4 addresses for 10 s each) and the stream's
setup (15 s). A running stream keeps the settings it started with.
A server that refuses the stream key is not tried again, except after a lost connection: a
destination whose server took the stream, lost the connection and then refuses the key is tried
again up to 3 times, waiting at least 5 s each time, and each of those tries counts toward the
number of tries. A refusal on the first connect, a refusal of the connection request, and a refusal
after the destination was stopped and started again are final. A destination stopped and started
again while it waits to try again or waits for the network tries at once and counts its failed
tries from none. The stream fails when no destination is sending or trying to.
While the stream runs, the status bar shows `RECONNECTING` with the stream's time, in the warning
color, when no destination is live and at least one waits to try again or waits for the network;
when some are live, it shows
`LIVE` followed by the count of those waiting, such as "1 reconnecting". A notice says "Lost the
connection to {name}; reconnecting" when a destination that is still trying loses its connection,
and "Reconnected to {name}" when it is live again. Each destination gets at most one loss notice a
minute (StudioCraft's own value), and a reconnect notice only after a loss notice; `session.events`
keeps every lost connection.
Each try looks the server's name up again, so a changed DNS record is used on the next try. A try
waits at most 10 s for the lookup and then counts as a failed try, and stopping a destination or
the stream does not wait for a lookup. The system may keep working on a lookup it never answers;
each destination has at most two lookups outstanding and StudioCraft at most 32, and a try that
finds either limit reached starts no lookup and counts as failed. An answer that arrives after its
try gave up is not used. A server given as an IP address is not looked up, so these limits do not
apply to it.
Before trying again, a destination asks the system whether it has a network route to the server.
The check sends nothing. For a server given as an IP address with no route, the destination shows
"Waiting for the network" and does not try; each longest wait spent without a route counts as a
failed try, so a long outage still ends the destination. For a server given by name, the
destination shows "Waiting for the network" when its last known addresses have no route and the
system has no default route, or when no address is known, the last try failed to look the name up
or to connect in time, and the system has no default route. It still tries at the usual time,
looking the name up again. When the system has a default route, a named server is tried at the
usual time. StudioCraft checks the route of the waiting destinations once a second, and a
destination whose route returns tries at once instead of finishing its wait. A route only means
that this computer has a path for the connection; the server may still be unreachable beyond it.
A key written at the end of the server address, as in `rtmp://host/app/key`, is used as the stream
key when the key field is empty, and the stream's status leaves it out of the address it shows.

The stream key in Settings › Stream is saved in the profile file as plain text. A destination can
instead use a key held only for the running session, which is never saved. A line under the key
field says where the first destination's key comes from: this profile, a key given for each session,
or no separate key, when a key at the end of the server address is used if there is one. Typing a
key in the field saves it in the profile.

View › Streaming opens the Streaming window, which lists every destination of the profile. Each
row shows the destination's on or off setting, its server, where its key comes from and what it is
doing. A key at the end of a server address, and any query, are shown as dots. While the stream
runs, each destination shows its own status: connecting, live, trying again with the cause, stopped
or failed with the cause, so one destination's error does not hide another's status. The cause
names the kind of failure: no network route, a name that could not be looked up, a refused or
unanswered connection, a certificate that was not accepted, a server that refused the stream, or a
connection lost after the stream started. Holding the pointer over the status shows the error as
the connection reported it and, when no address of the server accepted the connection, each address
tried (at most four) with its own outcome. A lookup with no answer within 10 s says the DNS server
did not answer. On Windows a lookup failure also says whether the DNS server answered that the name
does not exist or did not answer; on macOS and Linux a lookup the system answers with an error
reports only that the lookup failed. Start and
Stop send to one destination or stop sending to it while the others go on. A destination added
after the stream started is sent to from the next start, and so are changes to a server or a key.

Check Connection, under each destination of the Streaming window, checks what this computer's
network can reach on the way to the destination's server and shows one verdict, such as "The DNS
server did not answer for {host}" or "{host} answers on port 443 but not on port {port}". What the
check found opens the list of every step with its outcome. The check runs its steps at the same
time and reports after at most 20 s; a step still running then is listed as unfinished. Each
connection, secure handshake and fetch may take 5 s, and the name lookup 10 s. The steps are:

- The check asks the system whether it has a route to any address and to each of the server's
  addresses. This sends nothing.
- It lists which of the variables `HTTPS_PROXY`, `https_proxy`, `ALL_PROXY` and `all_proxy` are set,
  by name only. The check itself uses no proxy.
- It looks the server's name up again.
- It opens a TCP connection to up to four of the server's addresses on the destination's port and,
  for `rtmps://`, completes the secure handshake, which sends the server's host name. It then closes
  the connection. It sends no stream key, no RTMP data and no part of the address after the host.
- When the port is not 443, it does the same with a secure handshake on port 443 of those addresses.
- For a destination set up for a streaming service that publishes a list of its ingest servers and
  a status summary, it fetches both over HTTPS, following no redirect, looks two of the service's
  other ingest servers up and makes a secure connection to each on port 443. It reads at most
  256 KiB of the list and 64 KiB of the summary.

A destination is also checked by itself when its third try in a row fails and when it gives up,
once until it is live again or started again; the row then says it was checked by itself after the
destination failed. A server address or a stream key that is not valid is not checked. A check
that runs by itself contacts only the destination's server unless Settings › Stream › Connection
check › "Also check the service's server list and status page after a failure" is on; it is off by
default. Check Connection always includes the service's steps. Stopping the stream cancels the
checks of destinations that had not failed, and a check of a destination that failed goes on.
At most four checks run at once. A check that runs by itself while four are running waits, and
starts when one of them has finished; it is dropped when its destination goes live or is started
again first. Each destination keeps its latest check for the session. A check is no longer shown
once the scheme, host or port of the destination's server changes, or its service changes; a
change to the rest of the address keeps it.

Add Destination is a row of tiles under the count of destinations: one for each of seven streaming
services, in the order of the services list, and one for a custom server. A click on a tile adds a
destination for it, up to 16, and no menu opens over the destinations. Each tile shows the service's
logo under the same rules as a row, and the service's badge otherwise, over the service's name. The
custom server tile shows a plain broadcast mark. A screen
reader reads each tile as the destination it adds. While the profile holds 16 destinations every
tile is turned off, and its tooltip gives the limit. The first destination uses the profile's stream key; the others have no key of their own until one
is chosen, so the profile's key is not sent to another service. A service that documents one ingest
server for every account fills in that server. For the other services the details link to the
service's page that shows the account's server and key. Edit opens a destination's service, name,
server and key source. Choosing another service replaces a server that is empty or that is one of
the services' documented servers, and keeps any other server. A service without a shared server leaves the server
empty, and a destination saved for it without a server is turned off. The server field hides its text until Show is pressed.
The key source is chosen from three cards, and each card says where its key is kept. The profile's stream key is typed
in Settings › Stream and saved in the profile file as plain text. A key for this session only is held in memory until
it is forgotten or StudioCraft quits, and is not written to the profile. With no separate key, any key is the last
part of the server address, which is also saved in the profile file as plain text. A click, Space or Enter chooses a
card, and the arrow keys choose the card before or after the focused one. Screen readers read each card as a radio
button with its text as the description. The cards stand side by side when the details are wide enough, and one
above the other otherwise. A row
shows the service it streams to, and the row and the details show a service's logo only where the
service publishes one for this use, at no less than the size its guidelines allow (`ASSETS.md`).
Where a service's logo is not shown, the row and the details show its badge instead: the first
letter of the service's name on a tile tinted from the theme's soft accent, with a hue of its own
for each service. The letter keeps a contrast of at least 4.5:1 with the tile, in light and dark
themes. The badge in the details is 72 points square and links to the service's page about
streaming, as the logo does. Save closes the details once the change is made; a change that is refused
leaves them open with what was typed. A destination whose key is held for the session has a field for that key:
Hold keeps the typed key in memory for this session and Forget drops it. Remove deletes a
destination; one that is sending must be stopped first. Each change is saved in the profile; a change
that cannot be saved is undone. When Settings is open, the changed destination is also changed in
its Stream tab, and edits not yet applied there to other destinations stay. A new destination
never takes the id of one the running stream still holds.

The `golive.check` command lists what would stop the stream from starting or spoil it. It has an
item for each destination that is on, the stream's video encoder, the free space in the recording
folder, each source that is not working, and live captions. Each item passes, is a problem, or is
unknown, and a problem names the command that fixes it. A destination passes only while it is live,
or when it was live on an earlier stream of this session with the same server and key; one that has
not connected yet is unknown. Less than 10 GB free in the recording folder, a limit StudioCraft
chose, is a problem. Live captions that are off are a problem only when a Live Captions source is
in the scene collection ([Control protocol](control-protocol.md#golivecheck)).

Go Live to N, at the top of the Streaming window, where N is the number of destinations that are on,
runs the check and starts the stream (`stream.start`) when the check finds no problem. Items that
are unknown do not stop the stream, and the window lists them under "Not confirmed yet", so a
destination that has not connected is never shown as passing. When the check finds a problem, the
stream is not started and the window lists each problem. A problem fixed in Settings has an Open
Settings button, which opens the Output tab for the recording folder and the Stream tab for the
encoder. A source that is not working has Open Properties, and live captions that are off have
Turn On Live Captions, which runs `captions.start`. A problem with a destination is fixed in the
rows below. The button is off while the stream runs, while no destination is on and while the check
runs. The window keeps drawing while the free space in the recording folder is read, and shows
"Running the Go-live check…"; after 250 ms without an answer the free space is listed as not
confirmed, as `golive.check` lists it. Switching the profile or closing the window drops a check
that is still running. A result the
window cannot read, and a stream that fails to start, are shown as the reason the stream was not
started. After a start, the window shows "The stream is starting" until the stream runs, fails or
stops.

### Pause screen

File › Pause Program shows the pause screen in the program in place of the program scene, with
the scene collection's transition. Every recording, stream and replay buffer shows it too. File ›
Resume Program shows the program scene again with the same transition. File › Add a Minute to the
Pause adds 60 s to the countdown. Pause or Resume Program and Add a Minute to the Pause can be given
shortcuts in Settings › Hotkeys.

The Pause button sits after Go Live on the menu row. A click on it pauses with the profile's settings.
While paused, it is filled amber and reads Resume with the time left, or with the time paused when
there is no countdown, and a click resumes. Its arrow opens a menu: Pause for 1, 2, 5, 10 or 15
Minutes and Pause Without a Countdown pause with that countdown (while paused, they start the
countdown again from now), and Pause Screen Settings… opens Settings at the Stream tab with the
keyboard focus in the Pause screen section. The `pause.settings` window command does the same.

While paused, an amber banner above the program reads "Paused. Viewers see the pause screen." with
the time left ("Back in 4:32."), the time paused ("Paused for 2:10.") or "The countdown ended.", and
has +1 min and Resume buttons. Screen readers announce the banner when the pause starts and when the
countdown reaches zero, and announce "The program resumed" after Resume; they do not announce each
second. The status bar shows PAUSED with the time left, and the time paused on hover.

The pause screen draws, from the bottom up, a background color, one picture or one video, and the
countdown text. A video loops and plays without its sound, and a video file that holds only sound
shows nothing over the background. A GIF plays as a video, so an animated GIF loops; like other
videos it needs ffmpeg. The countdown text holds `{time}` once,
where the time left goes, as `4:07` or, from one hour, `1:02:07`. When the countdown reaches zero,
the pause screen either stays and shows the text at zero, or the program resumes. A countdown of 0
shows no text and does not end the pause.

Settings › Stream › Pause screen sets the picture or video, how it is sized to the canvas, the
background, the countdown and its text, what happens at zero, the text at zero, where the text
sits, its size as a share of the canvas height, its font, color and outline, and whether the
program audio is muted. The defaults are listed in
[MECHANICS.md](../MECHANICS.md#recording-and-streaming). Changes apply from the next pause.
`pause.start` can give a pause its own picture or video, countdown and choice at zero.

While the program audio is muted, every source the program hears fades out with the transition and
fades back in when the program resumes. Monitoring and each source's own audio track keep the
source's audio. A pause is not saved in the scene collection or the profile, and it is not an undo
step. A program scene switched to while paused shows when the program resumes. Pausing during the
fade back, or resuming during the fade in, turns a Fade or Fade through color around from the
picture it shows; other transitions start from the beginning. A picture or video
that can't be read is reported in the status bar, and the pause screen keeps its background and
text. A countdown text that can't be drawn is reported the same way. Settings whose countdown text
does not hold `{time}` exactly once are refused. While paused, switching to, creating, duplicating or
removing a profile is refused with "Resume before switching the profile".

## Hardware acceleration

Settings › Video › Hardware acceleration holds two settings.

**Graphics adapter** picks the adapter the window draws with: Automatic, Power saving, Software,
or a listed adapter. The choice takes effect from the next start. The window shows a notice when
the chosen adapter is missing or the app fell back to software. After a device loss the app starts
again on a new device, and after 3 losses in 10 minutes it draws in software.

**Hardware encoder** picks the encoder that recordings and streams use when their encoder is
Hardware: Automatic, Software only, or one of the listed hardware encoders. Software only uses
x264 without testing the hardware encoders. Automatic tests each candidate with a one-frame
encode, in an order set by the graphics adapter's vendor, and uses x264 when none works, or
H.264 (built in) for a stream. One test stops after 5 s, and the tests of one start stop after
10 s together; a candidate left untested is tested at the next start. The results are kept in
`hardware-encoder-tests.txt` in the settings folder, for the `ffmpeg` program, its version, the
graphics driver and the StudioCraft version they were found with. A change to any of them, or a
new graphics device after a device loss, tests the encoders again. Tools › Test Hardware Encoders
Again (the `hardware_encoders.retest` command) forgets the results and removes the file. The
candidates run in `ffmpeg`, except for a stream:

| Family | Codecs |
| --- | --- |
| NVIDIA NVENC | H.264, HEVC and AV1 |
| AMD AMF | H.264, HEVC and AV1 |
| Intel Quick Sync | H.264, HEVC and AV1 |
| Windows Media Foundation | H.264 and HEVC |
| VideoToolbox | H.264 and HEVC |
| VA-API | H.264, HEVC and AV1 |

For an MKV recording that is not at the Lossless level, StudioCraft tests each candidate that has
a built-in encoder in the following list with that built-in encoder, inside its own process
instead of in `ffmpeg`. When such a candidate is chosen, the recording uses the built-in encoder.
A stream tests only the H.264 candidates in this list, and skips every other candidate:

- NVIDIA NVENC H.264 and HEVC (built in) run through the NVIDIA driver on Windows and 64-bit
  Linux.
- Windows Media Foundation H.264 and HEVC (built in) run the hardware encoders Media Foundation
  lists, on Windows.
- Apple VideoToolbox H.264 and HEVC (built in) run the VideoToolbox hardware encoders, on macOS.
- VA-API H.264 and HEVC (built in) run the VA-API hardware encoders through libva, on 64-bit
  Linux. VA-API H.264 or HEVC chosen by name is tested the same way, and then records with the
  built-in encoder.

The compositor converts the program to the encoders' frame format on the GPU.

The compositor can also draw a small picture of each of up to 16 scenes, for scene lists. Each
picture fits a 256 × 144 box by default, keeps the canvas's aspect ratio, and is redrawn at most 4
times a second. The pictures stay on the GPU. Scenes past the sixteenth are not drawn and are
counted.

The [`scene.thumbnails`](control-protocol.md#scenethumbnails) command names the scenes to draw and
how often: 2 times a second by default. While no command has named any, or after one names none,
the compositor draws no scene pictures, so a scene list that shows only names adds no work to a
frame.

## Interface

### Theme

StudioCraft shows the suite theme of the Craft apps in dark mode with the Compact size set. Its
colors, sizes and type come from the theme's tokens. Controls, including Record and Go Live, are
24 points tall, dock headers are 28 points tall and the status bar is 24 points tall. The audio
meters keep their own green, yellow and red on a dark track in both light and dark mode, and each
has a contrast of at least 3:1 against the track.

### Menu row and output controls

The menu row is 32 points tall. It holds the menu titles at the start of the row, and at the end
an Edit and Studio switch, the transition picker with its duration, Record and Go Live with
their timers, and the Pause button with its menu arrow ([Pause screen](#pause-screen)). When the
row is too narrow, the transition picker moves behind an icon button first, then Record, Go Live and
Pause leave out their labels and keep the glyph and timer, and last the menu titles move into the
menu bar's overflow menu. In a right-to-left interface the row is mirrored. Docks › Output Controls and Docks › Transition Controls show and hide the two groups.

### Docks

Scenes, Sources and Audio Mixer sit in a strip under the preview, and Application Audio joins them
when Docks › Application Audio shows it. Each dock has a 28-point header
with the title at its start and the dock's actions at its end, and no footer. When the dock is
narrow, the header keeps its actions and shortens the title. 1-point dividers separate the docks.

The strip is 200 points tall, or 30% of the window's height when that is less, and never under 120
points. Scenes, Sources, Audio Mixer and Application Audio start 300, 340, 400 and 360 points wide
([MECHANICS.md](../MECHANICS.md#editing)), and the last shown dock takes the rest. No dock is
narrower than 200 points while the window has room.

You resize the strip by dragging its top edge outside the header actions, and resize the docks by
dragging a divider. The edge and the dividers have 8-point hit bands. The strip grows only while the
preview above it keeps at least 270 points of height, and it is never under 120 points.

A dock can also sit in a column on the left or the right of the preview. A right-click on a dock's
title offers Move to the Left Column, Move to the Right Column and Move Under the Canvas. The docks
in a column share its height, and a 1-point line separates them. The left column starts 256 points
wide and the right column 296 points. You resize a column by dragging its inner edge, between 200
and 480 points, and a drag stops where a wider column would leave the preview less than 480 points
of width. When the window is narrower than 900 points, or the columns would leave the preview
less than 480 points of width, the columns' docks are drawn in the strip with its own docks, all in
one order that Move Left, Move Right, Move Up and Move Down change, and they go back to their
columns when the window is wider again.

In the strip the right-click menu offers Move Left and Move Right, and in a column it offers Move Up
and Move Down. Each one passes over hidden docks and moves a dock only among the docks drawn with
it. The menu also offers Undock.

An undocked dock is drawn in a window of its own inside the main window, with a title bar above the
dock's header. You drag the window by its title bar and resize it from any edge or corner. A resize
from the left or top edge moves that edge, and the window keeps its new place. A dock without a saved
place opens centered in the main window; Sources, Audio Mixer and Application Audio open 24, 48 and
72 points further down and to the right, as far as the main window has room. Its starting and smallest sizes are in [MECHANICS.md](../MECHANICS.md#editing). When
the main window is smaller, StudioCraft draws the undocked dock further in without changing its saved
place or size. A right-click on the dock's title offers Dock in the Strip, which puts the dock under
the canvas, also when it was undocked from a column.

StudioCraft keeps the strip's height, the columns' widths, the docks' widths, visibility, order and
places, which docks are undocked, and their windows' places and sizes until Docks › Reset Docks.
Reset Docks puts every dock back in the strip in its first order.

### Scene grid

The first button in the Scenes dock's header switches the dock between its list and a grid of
scene tiles, and StudioCraft keeps the choice. The list is shown until you switch. The grid has two
tiles to a row, in the list's order, and an Add Scene tile last. Each tile shows the scene's
picture over its name. The first nine tiles also show the shortcut that shows their scene, as
Settings › Hotkeys sets it; a scene whose shortcut was cleared shows none. In studio mode the
program scene's tile has a red border and a PGM chip, and the preview scene's tile has an accent
border and a PVW chip. Outside studio mode the selected scene's tile has the accent border. A
tile shows an empty picture until its scene's first picture is drawn. On a narrow tile, a chip or
shortcut that does not fit is not drawn.

A click on a tile selects its scene, as in the list. A double-click sends the scene to the program
in studio mode and asks for a new name otherwise, and a right-click opens the scene's menu. A
screen reader reads each tile as the scene's name, its place in the list and its marks, for
example "Scene, Talk, 2, in program". The highlight under the pointer fades in over the theme's
hover time, and appears at once when the suite's reduced motion setting is on.

While the grid is shown, it asks for the pictures of the scenes whose tiles are on screen, at most
16, with [`scene.thumbnails`](control-protocol.md#scenethumbnails) at 2 pictures a second. When
the dock switches back to the list or is hidden, the pictures the grid asked for stop, unless a
control client asked for pictures after the grid did. The list draws no scene pictures.

### Dock layouts

Docks › Save Dock Layout… asks for a name and keeps every dock setting under it. Docks › Layouts
puts a layout back, and Docks › Remove Layout deletes one. Layout names follow the rules for
profile names and are compared without regard to case, so saving under a name that is taken
replaces that layout. At most 32 layouts are kept, and a settings file with more keeps the first
32.

Docks › Layouts also lists four built-in layouts, above the saved ones:

- **Set up**: Scenes and Sources in a 256-point left column, and Audio Mixer under the preview.
- **Live**: Sources and Audio Mixer in a 268-point left column, and Scenes under the preview.
- **Review**: Scenes in a 256-point left column, and Audio Mixer and Application Audio under the
  preview, with Sources hidden.
- **Classic**: every dock in the strip, as Reset Docks leaves them.

The built-in layouts cannot be changed or removed, and saving under one of their names is refused.
A saved layout from an earlier version whose name is now a built-in layout's is kept as
"<name> (saved)".

### Fonts

The interface uses Inter for text, Inter SemiBold for titles and JetBrains Mono for readouts. When
`CRAFT_FONTS_DIR` is set at build time, the craft-fonts faces follow for the scripts Inter lacks,
and egui's built-in faces come last. When the interface language is written in a script Inter
lacks (Arabic, Thai), that script's face goes first. StudioCraft draws the menus' check and radio
marks with or without `CRAFT_FONTS_DIR`.

### Title bar

The menu strip is the window's title bar on Windows, on macOS (with the window buttons over its
start) and on Linux desktops that float their windows. You move the window by the strip's empty
part, a double-click on it maximizes or restores the window (on macOS it does what the system's
title bar setting says), and the window's edges resize it. On Windows and Linux the window buttons
sit at the ends of the strip in the desktop's order, sized and drawn as the desktop draws its own.
Tiling window managers keep the system's title bar. Settings › General › "Use system title bar"
keeps the system's title bar everywhere, from the next start. `STUDIOCRAFT_TITLEBAR=app` or
`=system` overrides the choice, and `STUDIOCRAFT_CAPTION_STYLE` (`windows`, `adwaita`, `breeze` or
`generic`) and `STUDIOCRAFT_CAPTION_LAYOUT` pick the look and order of the window buttons.

### Window size and place

The first time StudioCraft starts, its window is 1280 by 800 points. On a screen too small for
that, the window shrinks to fit the screen and opens maximized. The window can't be made smaller
than 760 by 480 points, unless the screen itself is smaller; at that size every dock header, the output bar and the status bar still
show in full. StudioCraft remembers the window's
size, position and whether it was maximized, and opens it the same way at the next start. A
window saved on a monitor that is no longer connected opens on a connected one, and a window
larger than its monitor shrinks to fit it. On Wayland the desktop places the window. Starts with
`--no-prefs` always open at the first size.

### Languages

View › Language and Settings › General › Language pick the interface language, or follow the
system language with Automatic. On macOS the Language submenu is in the app menu. Each language is
listed by its own name, and the interface switches without a restart. These languages are built
in: English, Simplified Chinese, Traditional Chinese, Arabic, Czech, Dutch, French, German, Hebrew,
Italian, Japanese, Korean, Polish, Brazilian Portuguese, Russian, Spanish, Swedish, Turkish and
Ukrainian.

In Arabic and Hebrew the window runs right to left. The menus, output controls, dock headers and
status bar start at the right, and egui's submenus, scroll bars and collapsing headers mirror.

### Profiles and scene collections

A profile is a set of stream and output settings, and a scene collection is a set of scenes. You
can keep several of each and switch between them from the menu bar. Scene Collection › Import…
and Export… read and write a scene collection as a file.

### Stats and scopes

View › Stats shows the frame count, late frames, frame times, encoded and skipped frames, the
encoder queue, and the size of the running recording or stream.

View › Scopes shows a waveform, an RGB parade, a histogram, or the YUV or HLS vectorscope of the
program output. The `scopes.kind` command picks the scope.

### Help and updates

Help › Community opens the Discord server, the website, the source code and the issue tracker.
Help › Check for Updates… looks for a newer release, and Settings › General › Check for updates at
start (on by default) does the same when the app starts. On Windows, a copy of the program
finishes installing a downloaded update, starts StudioCraft again and exits. Help › About
StudioCraft lists the contributors, with a choice of username, display name or real name, and
shows the license texts.

The app icon is the window icon, the picture in the update prompt, and the icon of the menu entry
the updater writes beside an AppImage.

## Hotkeys

Settings › Hotkeys lists every command that can take a shortcut and changes its shortcuts. Type
shortcuts such as Ctrl+1 or Shift+F5, and separate several with commas. Ctrl+1 to Ctrl+9 on
Windows and Linux, and ⌘1 to ⌘9 on macOS, show the first nine scenes. The list includes Start or
Stop Recording, Start or Stop Streaming, Start or Stop Replay Buffer, Save Replay, Transition to
Program, Cut to Program, Studio Mode and the commands that swap a
[staged window or display](#staged-windows).

## Settings

Edit › Preferences… on Windows and Linux, and StudioCraft › Settings… (⌘,) on macOS, open the
Settings window. OK or Apply puts the changes in force, and Cancel
discards them.

| Tab | Settings |
| --- | --- |
| General | Language, Check for updates at start, and Use system title bar (from the next start). |
| Stream | Server, Stream key, Video bitrate, Audio bitrate, Video encoder and Keyframe interval, and the Pause screen section ([Pause screen](#pause-screen)). |
| Output | Recording folder, Format, Quality, Video encoder and its settings, the `ffmpeg` program, Replay buffer length and Replay buffer memory limit. |
| Audio | Monitoring device, the device monitored sources play on. |
| Video | Canvas size (the presets include 1080 × 1920 for vertical video), Match display (sets the canvas size to the display the window is on or to a listed display. Once applied, the canvas follows that display when its resolution or scaling changes, until the canvas size is set another way. The display the window is on is followed only when no other listed display has its size), Output size, Output scale (100%, 75%, 66% or 50% of the canvas; an output size at one of these shares stays at that share when the canvas size changes) and Frames per second, and the Hardware acceleration section with Graphics adapter and Hardware encoder. Video settings can't change while recording, streaming or running the replay buffer. |
| Hotkeys | A shortcut for each command. |

## Control and automation

StudioCraft runs every action as a command with a stable id. Three tools run those commands from
outside the window:

- `studiocraft-cli send <port> <method> [<params>]` sends one control request to the running app
  and prints the reply. `studiocraft-cli check <file>` reports problems in scene collection files.
- `studiocraft --control <port>` accepts JSON requests on `127.0.0.1:<port>`. Each request carries
  the token from `STUDIOCRAFT_CONTROL_TOKEN`. [docs/control-protocol.md](control-protocol.md)
  describes the protocol, and its [Command line](control-protocol.md#command-line) section shows
  how to make the token.
- The Model Context Protocol (MCP) server lets an MCP client drive StudioCraft.
  `studiocraft-cli mcp` runs a StudioCraft engine in its own process without a window, and
  `studiocraft-cli mcp --connect <port>` drives the app started with `--control <port>`, including
  its window, menus and screenshots. Connected to the app, the server can inspect the window, take
  screenshots, click, drag, press keys, type, use menus, answer dialogs and quit.
  [docs/mcp.md](mcp.md) describes how to register it, and its tools, resources and prompts.

In a POSIX shell, these commands start the app with a control port, add a scene and connect the
MCP server:

```sh
export STUDIOCRAFT_CONTROL_TOKEN=$(openssl rand -hex 32)
studiocraft --control 9876 &
studiocraft-cli send 9876 engine.execute '{"command": "scene.add", "params": {"name": "Intro"}}'
studiocraft-cli mcp --connect 9876
```

### Command-line options

```text
studiocraft [--config-dir <dir> | --no-prefs] [--control <port>]
studiocraft --screenshot <file.png>
studiocraft --self-test <file.json>
studiocraft --version
```

| Option | What it does |
| --- | --- |
| `--config-dir <dir>` | Keeps settings, scene collections and profiles in `<dir>`. |
| `--no-prefs` | Keeps everything in memory, and reads and writes no settings. |
| `--control <port>` | Accepts control requests on `127.0.0.1:<port>`. |
| `--screenshot <file>` | Draws about 120 frames, saves the window as a PNG and exits. |
| `--self-test <file>` | Runs a short scripted session and writes its steps as JSON. |

The command-line tool `studiocraft-cli` sends control requests, runs the MCP server, checks scene
collection files and times the compositor without a window. To list its commands from a source
checkout, run:

```sh
cargo run --release -p studiocraft-cli -- help
```
