# Features

This page describes everything StudioCraft does, one section per area. It is for people who use
the app and want to know what a menu item or a setting does. The tuned values, such as timings,
sizes and bitrates, are listed in [MECHANICS.md](../MECHANICS.md).

## Sources

A scene is a stack of sources. You add, rename, duplicate, reorder and remove scenes and sources.
In the preview you drag, resize and rotate sources. A source that is not rotated snaps when an
edge or its center line comes within 8 points of a canvas edge, a canvas center line, or an edge or
center line of another visible source, and a line shows what it snapped to. A move snaps on each
axis. A corner drag snaps the edges it moves and keeps the aspect ratio: the edge closer to its
line snaps and the other edge follows the ratio. With Shift held the corner drag changes the ratio
and each edge snaps on its own. Holding Alt (Option on macOS) turns snapping off. One drag is one
undo step. You can lock a source so it can't be moved, or lock the whole preview. Each scene item can be
shown or hidden, and every change can be undone.

### Built-in sources

| Source | What it captures | Systems |
| --- | --- | --- |
| Color | A solid color. | All |
| Image | A picture file. | All |
| Text | Text in a chosen font. | All |
| Media file | A media file. | All |
| Display capture | A whole display. | Windows, macOS and Linux |
| Window capture | One window. | Windows, macOS and Linux |
| Camera | A webcam or capture card. | Windows (Media Foundation), macOS (AVFoundation) and Linux (V4L2) |
| Audio input capture | A microphone or other input device. | Windows, macOS and Linux |
| Audio output capture | The sound the computer plays (desktop audio). | Windows and Linux |
| Application audio capture | The sound one application plays. | Windows 11 and Windows Server 2022 |
| Live Captions | The words heard in the program audio, while live captions run. | All |

### Display and window capture on Linux

On Linux, display and window capture go through the desktop's screen-cast portal
(`xdg-desktop-portal` with a backend for your desktop) and PipeWire. The desktop's own picker
asks for the display or window when the source starts. After the picker is canceled or sharing is
stopped, changing any of the source's properties opens the picker again.

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
which needs Windows build 20348 or later: Windows 11 or Windows Server 2022. On earlier Windows
builds, macOS and Linux the source lists no applications and says why, and Audio output capture
still records everything the computer plays. The `app_audio.list` and `app_audio.status` commands
report the same lists and support ([Control and automation](#control-and-automation)).

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
| A second camera source shows the same camera | One camera shows in one camera source at a time. The second source reports that the camera is in use by another camera source. |
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
| Gain, noise gate, compressor, limiter and equalizer | Change the level and tone of a source's audio. |

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
button for every audible source. The mixer also measures the final mix's loudness (momentary,
short-term, integrated, loudness range and true peak). The window does not show these readings
yet.

A source's Properties window sets two more audio options:

- **Sync offset** delays the source's audio by 0 to 5000 ms to line it up with its picture.
- **Monitoring** is off, monitor only, or monitor and output. With monitor only, you hear the
  source, and recordings and streams leave it out.

Monitored sources play on the device chosen in Settings › Audio. Monitoring works on Windows and
macOS.

### Application Audio dock

Docks › Application Audio shows a dock that lists a lane for each audio input source, then a lane
for each running application that has audio. Each lane has a live meter. A lane the program does
not hear is drawn gray. The meter of a lane with a source shows the source's level in the mix,
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

A lane with a source has a volume slider in dB and a mute button, and its settings menu opens the
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
only the audio tracks, and the status bar gives the reason. The take's files keep every whole frame
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
started, the take's folder opens instead and the status bar gives the reason. A take that ends
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
A lost connection is tried again after 2, 4, 8 and 16 s and then every 30 s, and a destination gives
up after 10 failed tries in a row. The stream fails when no destination is sending or trying to.
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
runs, each destination shows its own status: connecting, live, trying again with the error, stopped
or failed with the error, so one destination's error does not hide another's status. Start and
Stop send to one destination or stop sending to it while the others go on. A destination added
after the stream started is sent to from the next start, and so are changes to a server or a key.

Add Destination adds one, up to 16. The first destination uses the profile's stream key; the
others have no key of their own until one is chosen, so the profile's key is not sent to another
service. Edit opens a destination's name, server and key source. The server field hides its text
until Show is pressed. Save closes the details once the change is made; a change that is refused
leaves them open with what was typed. A destination whose key is held for the session has a field for that key:
Hold keeps the typed key in memory for this session and Forget drops it. Remove deletes a
destination; one that is sending must be stopped first. Each change is saved in the profile; a change
that cannot be saved is undone. When Settings is open, the changed destination is also changed in
its Stream tab, and edits not yet applied there to other destinations stay. A new destination
never takes the id of one the running stream still holds.

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

## Interface

### Theme

StudioCraft shows the suite theme of the Craft apps in dark mode with the Compact size set. Its
colors, sizes and type come from the theme's tokens. Controls, including Record and Go Live, are
24 points tall, dock headers are 28 points tall and the status bar is 24 points tall. The audio
meters keep their own green, yellow and red on a dark track in both light and dark mode, and each
has a contrast of at least 3:1 against the track.

### Menu row and output controls

The menu row is 32 points tall. It holds the menu titles at the start of the row, and at the end
an Edit and Studio switch, the transition picker with its duration, and Record and Go Live with
their timers. When the row is too narrow, the transition picker moves behind an icon button
first, then Record and Go Live leave out their labels, and last the menu titles move into the menu
bar's overflow menu. Docks › Output Controls and Docks › Transition Controls show and hide the two groups.

### Docks

Scenes, Sources and Audio Mixer sit in a strip under the preview, and Application Audio joins them
when Docks › Application Audio shows it. Each dock has a 24-point header
with the title at its start and the dock's actions at its end, and no footer. When the dock is
narrow, the header keeps its actions and shortens the title. 1-point dividers separate the docks.

The strip is 200 points tall, or 30% of the window's height when that is less, and never under 120
points. Scenes, Sources, Audio Mixer and Application Audio start 300, 340, 400 and 360 points wide
([MECHANICS.md](../MECHANICS.md#editing)), and the last shown dock takes the rest. No dock is
narrower than 200 points while the window has room.

You resize the strip by dragging its top edge outside the header actions, and resize the docks by
dragging a divider. The edge and the dividers have 8-point hit bands, and a dragged strip can be up
to 45% of the window's height. A right-click on a dock's title offers Move Left and Move Right,
which pass over hidden docks, and Undock.

An undocked dock is drawn in a window of its own inside the main window. You drag it by the dock's
title and resize it from its right and bottom edges. Its starting and smallest sizes are in
[MECHANICS.md](../MECHANICS.md#editing). When the main window is smaller, StudioCraft draws the
undocked dock further in without changing its saved place or size. Its menu offers Dock in the
Strip.

StudioCraft keeps the strip's height, the docks' widths, visibility and order, which docks are
undocked, and their windows' places and sizes until Docks › Reset Docks. Reset Docks puts every
dock back in the strip in its first order.

### Dock layouts

Docks › Save Dock Layout… asks for a name and keeps every dock setting under it. Docks › Layouts
puts a layout back, and Docks › Remove Layout deletes one. Layout names follow the rules for
profile names and are compared without regard to case, so saving under a name that is taken
replaces that layout. At most 32 layouts are kept, and a settings file with more keeps the first
32.

### Fonts

The interface uses Inter for text, Inter SemiBold for titles and JetBrains Mono for readouts. When
`CRAFT_FONTS_DIR` is set at build time, the craft-fonts faces follow for the scripts Inter lacks,
and egui's built-in faces come last. When the interface language is written in a script Inter
lacks (Arabic, Thai), that script's face goes first. StudioCraft draws the menus' check and radio
marks with or without `CRAFT_FONTS_DIR`.

### Title bar

StudioCraft draws its own title bar only where the desktop does not draw a window frame (GNOME on
Wayland, or `STUDIOCRAFT_TITLEBAR=app`) and Settings › General › "Use system title bar" is off.
The menu strip is then the title bar. You move the window by its empty part, a double-click
maximizes it, and the window buttons sit where the desktop puts them.

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
Program, Cut to Program and Studio Mode.

## Settings

Edit › Preferences… on Windows and Linux, and StudioCraft › Settings… (⌘,) on macOS, open the
Settings window. OK or Apply puts the changes in force, and Cancel
discards them.

| Tab | Settings |
| --- | --- |
| General | Language, Check for updates at start, and Use system title bar (from the next start). |
| Stream | Server, Stream key, Video bitrate, Audio bitrate, Video encoder and Keyframe interval. |
| Output | Recording folder, Format, Quality, Video encoder and its settings, the `ffmpeg` program, Replay buffer length and Replay buffer memory limit. |
| Audio | Monitoring device, the device monitored sources play on. |
| Video | Canvas size, Match display (sets the canvas size to the display the window is on or to a listed display; on Linux only the window's display is offered), Output size and Frames per second, and the Hardware acceleration section with Graphics adapter and Hardware encoder. Video settings can't change while recording, streaming or running the replay buffer. |
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
