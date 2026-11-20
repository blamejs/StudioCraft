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
- The mixer measures the final mix's loudness, and the window does not show the readings yet ([Audio](docs/features.md#audio)).
- Audio monitoring works on Windows and macOS, with each source off, monitor only or monitor and output ([Audio](docs/features.md#audio)).

### Recording

- MKV files are recorded with the built-in H.264 and AAC encoders without ffmpeg ([Encoders](docs/features.md#encoders)).
- MKV, MP4 and MOV files, including recordings at the Lossless quality level, are recorded through an external ffmpeg ([When `ffmpeg` is needed](docs/features.md#when-ffmpeg-is-needed)).
- The replay buffer keeps the last 5 to 3600 seconds in memory and saves them to an MKV file while it keeps running ([Replay buffer](docs/features.md#replay-buffer)).
- Each audio source and the program mix are written to their own WAV files while a recording or the stream runs, and takes cut short by a crash are repaired ([Audio tracks](docs/features.md#audio-tracks)).
- Each take of audio tracks writes a session file for the audio editor, with markers set during the take and dropouts marked, and can open it in the editor when it ends ([Audio tracks](docs/features.md#audio-tracks)).

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

1. **Translations.** A `studiocraft.tsv` catalog will be added for every other locale craft-i18n
   has. Right-to-left layout will extend to the dock order, the order of the menu titles and the
   Settings, Properties and Filters windows. Today egui's `MenuBar` lays the menu titles out left
   to right in every language.
2. **Multiple outputs (in progress).** StudioCraft will stream to several destinations at once and
   offer a virtual camera. `craft-flv` already writes and reads the FLV tags and AMF0 values an
   RTMP stream carries. `craft-packet` shares each encoded packet by reference with every
   destination's bounded ring. `craft-rtmp` publishes a stream to an RTMP or RTMPS server.
3. **Web build.** The model and plugin crates already build for WebAssembly. A browser editor for
   scene collections is the first step.
4. **Frames on the GPU from capture to the encoder.** Captured frames will stay on the GPU until
   the encoder reads them.
5. **Application Audio panel.** A dock will list the running applications as lanes with live
   meters: a lane routed to the mix is shown in color, and one that is not is grayed out while its
   meter keeps moving. Each application and microphone lane gets enable and route switches, gain
   with boost above 0 dB, filters, a sync offset, track assignment, mute and monitoring. The
   `app_audio` source and the application list it reads are already built in.
6. **Application audio on macOS and Linux.** The application list, meters and capture will use
   ScreenCaptureKit on macOS and PipeWire's per-application streams on Linux. Both systems report
   it as unsupported today.
