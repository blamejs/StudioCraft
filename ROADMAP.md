# Roadmap

This page lists what StudioCraft does today and what comes next. Each shipped item links to its
description in [docs/features.md](docs/features.md).

## Shipped

### Sources

- Scenes, sources and scene items can be moved, resized, rotated, locked, hidden and undone ([Sources](docs/features.md#sources)).
- Color, image, text, media file, display capture, window capture, camera, audio input and audio output sources are built in ([Built-in sources](docs/features.md#built-in-sources)).
- The media file source plays Ogg Vorbis files at 8 to 192 kHz without ffmpeg, through `craft-audio`'s Vorbis decoder, and mixes files with more than two channels down to stereo ([When `ffmpeg` is needed](docs/features.md#when-ffmpeg-is-needed)).
- Cameras are captured on Windows (Media Foundation), macOS (AVFoundation) and Linux (V4L2), with the camera and its format chosen in the source's Properties ([Camera](docs/features.md#camera)).
- Audio input and output are captured on Linux through the PipeWire sound server, which lists its devices with the default first ([Audio capture on Linux](docs/features.md#audio-capture-on-linux)).
- Displays and windows are captured on Linux through the desktop's screen-cast portal and PipeWire ([Display and window capture on Linux](docs/features.md#display-and-window-capture-on-linux)).
- One application's audio is captured on Windows 11 and Windows Server 2022, chosen from the running applications that have audio and followed across restarts ([Application audio capture](docs/features.md#application-audio-capture)).

### Filters

- Color correction with white balance, chroma key, color key, crop and pad, image mask, scroll, sharpen, gain, noise gate, compressor, limiter and a three-band equalizer are built in ([Filters](docs/features.md#filters)).

### Transitions

- Cut, fade, fade through a color and swipe transitions are built in, with an Easing setting on the fades and the swipe ([Transitions](docs/features.md#transitions)).
- Studio mode sends the preview to the program with a transition or a quick cut or fade ([Studio mode](docs/features.md#studio-mode)).

### Audio

- The audio mixer has meters, faders and mute, and each source has a sync offset of 0 to 5000 ms ([Audio](docs/features.md#audio)).
- The Application Audio dock lists the microphones and the running applications that have audio as lanes with live meters, and routes an application into the program in every scene, together with the microphone ([Application Audio dock](docs/features.md#application-audio-dock)).
- The mixer measures the final mix's loudness, and the window does not show the readings yet ([Audio](docs/features.md#audio)).
- Audio monitoring works on Windows and macOS, with each source off, monitor only or monitor and output ([Audio](docs/features.md#audio)).

### Recording

- MKV files are recorded with the built-in H.264 and AAC encoders without ffmpeg ([Encoders](docs/features.md#encoders)).
- MKV, MP4 and MOV files, including recordings at the Lossless quality level, are recorded through an external ffmpeg ([When `ffmpeg` is needed](docs/features.md#when-ffmpeg-is-needed)).
- The replay buffer keeps the last 5 to 3600 seconds in memory and saves them to an MKV file while it keeps running ([Replay buffer](docs/features.md#replay-buffer)).
- Each audio source and the program mix are written to their own WAV files while a recording or the stream runs, and takes cut short by a crash are repaired ([Audio tracks](docs/features.md#audio-tracks)).

### Streaming

- Streams go to an RTMP server through an external ffmpeg, with the encoder's settings in Settings › Stream ([Streaming](docs/features.md#streaming)).

### Hardware acceleration

- Settings › Video › Hardware acceleration chooses the graphics adapter, and the app recovers from a device loss ([Hardware acceleration](docs/features.md#hardware-acceleration)).
- The compositor converts the program to NV12 on the GPU for the encoders (`craft-gpu-yuv`) ([Hardware acceleration](docs/features.md#hardware-acceleration)).
- The Hardware video encoder runs the hardware encoder set in Settings › Video › Hardware acceleration. Automatic tests each candidate with one frame, in an order set by the adapter's vendor, and falls back to x264 (`craft-hwenc`) ([Hardware acceleration](docs/features.md#hardware-acceleration)).
- NVIDIA NVENC, Windows Media Foundation, Apple VideoToolbox and VA-API H.264 and HEVC (built in) record MKV files in StudioCraft's own process (`craft-hwenc-native`) ([Hardware acceleration](docs/features.md#hardware-acceleration)).

### Interface

- A compact dark and light theme has a focus ring for keyboard navigation ([Theme](docs/features.md#theme)).
- The output controls sit on the menu row and adapt to a narrow window ([Menu row and output controls](docs/features.md#menu-row-and-output-controls)).
- The Scenes, Sources and Audio Mixer docks can be resized, reordered, undocked and saved as named layouts ([Docks](docs/features.md#docks)).
- The interface fonts come from `craft-ui`'s font stack ([Fonts](docs/features.md#fonts)).
- Nineteen interface languages are built in, and Arabic and Hebrew run right to left ([Languages](docs/features.md#languages)).
- Profiles and scene collections can be switched, and Scene Collection › Import… and Export… read and write collection files ([Profiles and scene collections](docs/features.md#profiles-and-scene-collections)).
- The stats window and View › Scopes show the program's timing and levels ([Stats and scopes](docs/features.md#stats-and-scopes)).
- Help › Check for Updates…, the check at start and the step that finishes installing a downloaded update on Windows run through `craft-update` ([Help and updates](docs/features.md#help-and-updates)).
- Help › About StudioCraft shows the credits, with a choice of username, display name or real name, and the license texts ([Help and updates](docs/features.md#help-and-updates)).
- The app icon in `assets/app-icon` is the window icon, the picture in the update prompt and the icon of the AppImage menu entry ([Help and updates](docs/features.md#help-and-updates)).

### Hotkeys

- Shortcuts can be changed in Settings › Hotkeys ([Hotkeys](docs/features.md#hotkeys)).

### Settings

- The Settings window has General, Stream, Output, Audio, Video and Hotkeys tabs ([Settings](docs/features.md#settings)).

### Control and automation

- The control channel and the Model Context Protocol (MCP) server run every command from outside the window ([Control and automation](docs/features.md#control-and-automation)).
- When the MCP server is connected to the app (`studiocraft-cli mcp --connect <port>`), it can inspect, screenshot, click, drag, press keys, type, use menus, answer dialogs and quit ([Control and automation](docs/features.md#control-and-automation)).

## Next

1. **Translations (partly done).** Nineteen interface languages are built in. In Arabic and Hebrew
   the menus, output controls, dock headers and status bar start at the right
   ([Languages](docs/features.md#languages)). A `studiocraft.tsv` catalog will be added for every
   other locale craft-i18n has. Right-to-left layout will extend to the dock order, the order of
   the menu titles and the Settings, Properties and Filters windows. Today egui's `MenuBar` lays
   the menu titles out left to right in every language.
2. **Multiple outputs (partly done).** StudioCraft will stream to several destinations at once,
   each started, stopped and reconnected on its own, and offer a virtual camera. The crates for
   it are built: `craft-flv` writes and reads the FLV tags and AMF0 values an RTMP stream
   carries, `craft-packet` shares each encoded packet by reference with every destination's
   bounded ring, and `craft-rtmp` publishes a stream to an RTMP or RTMPS server. The app does not
   use them yet, and a stream still goes to one server through `ffmpeg`. The list of
   destinations, publishing from StudioCraft's own process and the virtual camera are still to
   be built.
3. **Web build.** The model and plugin crates already build for WebAssembly. A browser editor for
   scene collections is the first step.
4. **Frames on the GPU from capture to the encoder.** Captured frames will stay on the GPU until
   the encoder reads them.
5. **Application Audio panel (partly done).** The Application audio capture source records one
   application's audio on Windows 11 and Windows Server 2022
   ([Application audio capture](docs/features.md#application-audio-capture)). The Application
   Audio dock lists microphones and running applications as lanes with live meters, grays a lane
   the program does not hear, and routes an application into every scene
   ([Application Audio dock](docs/features.md#application-audio-dock)). Each lane has volume up to
   0 dB, mute, filters, a sync offset and monitoring. Gain with boost above 0 dB and track
   assignment are still to be built.
6. **Application audio on macOS and Linux.** The application list, meters and capture will use
   ScreenCaptureKit on macOS and PipeWire's per-application streams on Linux. Both systems report
   it as unsupported today.
7. **Audio tracks as an editing session.** Each take's WAV files will also be written as a
   `.scraft` session that SoundCraft opens, with the tracks lined up at their times and the
   take's markers and dropouts marked.
8. **Streaming panel.** A dockable panel will show the status of each destination and start or
   stop all of them, a group or one with a single click, after checking the settings first. Each
   kind of service adds what its public interface offers, such as a list of ingest servers, the
   title and category, stream health, the viewer count and chat.
9. **Markers for editing.** A hotkey will drop a marker while a recording or the stream runs, and
   the recording will open in FilmCraft with the markers on its timeline.
10. **Raw and on-demand recordings.** A recording will be written twice: a raw copy for editing,
    with its tracks kept apart, and a copy that matches the stream for posting as a video on
    demand.
11. **Live captions.** Speech from an audio source or the mix will be turned into text on the
    computer while a recording or the stream runs. A Live Captions source shows the text on the
    program, or on the preview only, the stream carries it as closed captions, and `.srt` and
    `.vtt` files are written beside the recording.
12. **Canvas size from the display, and snapping while resizing.** Settings › Video will offer a
    canvas that matches a chosen display and follows it when the display's resolution changes,
    with common sizes as presets. Today a resized item keeps its aspect ratio unless Shift is
    held, and an unrotated item snaps to the canvas edges and center only while it is moved.
    Snapping will also work while an item is resized, keeping the aspect ratio, and items will
    snap to each other's edges.
13. **AMD and Intel encoders in StudioCraft's own process, and hardware decoding.** The AMD AMF and
    Intel Quick Sync encoders run only through `ffmpeg` today. They will run in StudioCraft's own
    process like the NVENC, Media Foundation, VideoToolbox and VA-API encoders. Media file sources
    will decode video on the GPU, with software decoding as the fallback.
14. **Interface refinements.** Scene and source lists will get compact rows with type icons, and
    show eye and lock buttons on hover. The icons will be redrawn on one grid, and the audio mixer
    will get new meters and faders. Studio mode and the menus and context menus, with their
    shortcuts and icons, will be laid out again. The Settings window will become a fixed-size
    dialog with side navigation, and the other windows will open centered. A command palette
    (Ctrl+K), a grid of scenes with thumbnails, hover fades and notifications that fade out will
    be added.
15. **Properties inspector.** A dock will edit the selected item's position and size, source
    settings, filters and audio in place, and every change applies at once and can be undone. A
    dock layout with the canvas in the middle, the sources on one side and the inspector on the
    other will be offered beside the current layout.
16. **Staged windows.** Several windows or displays can be picked ahead of time and swapped onto
    the program with a click, a hotkey or a command, without a scene for each. Staged windows
    keep capturing in the background, so the new window shows on the next frame. A window that
    closes or minimizes shows a placeholder instead of the desktop behind it.
