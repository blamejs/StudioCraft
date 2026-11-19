# Getting started

This page takes you from a download to your first recording and your first stream. It is for
people who install StudioCraft from a release.

## Install from a release

Download the file for your system from the
[Releases page](https://github.com/blamejs/StudioCraft/releases). StudioCraft runs on Windows,
macOS and Linux.

| System | File to pick |
| --- | --- |
| Windows x64 and Arm64 | `studiocraft-<version>-windows-<arch>.msi` installs StudioCraft. `studiocraft-<version>-windows-<arch>-portable.zip` runs without installing. |
| macOS (Apple silicon and Intel) | `studiocraft-<version>-macos-universal.dmg` holds the app. `studiocraft-cli-<version>-macos-universal.zip` holds the command-line tool. |
| Linux x86_64 and aarch64 | The `.AppImage` runs without installing. The `.deb` and `.rpm` install a system package, the `.flatpak` installs a Flatpak bundle, and the `.tar.gz` holds the files to unpack anywhere. |

The Windows files and the macOS app are signed, and the macOS app is notarized. In every alpha
release, the Linux `.rpm`, `.deb` and `.flatpak` packages are signed with the release key in
`release-signing-key.asc`, and every Linux file has a detached `.asc` signature beside it. A regular
release carries these Linux signatures only when it was signed with the release key. An alpha's
release notes show how to verify the downloads against `SHA256SUMS.txt` and `SHA3-512SUMS.txt`.

No package includes `ffmpeg`. You need it for streaming, for some kinds of recording, as
[First stream](#first-stream) describes, and for Media file sources. The `.deb` and `.rpm` packages list `ffmpeg` as a
recommended package, so `apt` and `dnf` usually install it with them. The Flatpak runtime has no
`ffmpeg`, and the Flatpak sandbox cannot run an `ffmpeg` installed on the system.

## First recording

A new scene collection already has two audio sources that every scene records:

- **Desktop audio** captures the sound the computer plays, on Windows and Linux. On macOS this
  source records nothing: StudioCraft captures an output device only on Windows and Linux.
- **Microphone** captures the system's default input device.

Each one shows in the Audio Mixer dock with a meter and a mute button. Mute a source there to
leave it out of the recording. Do not add another Audio output capture source for the same device,
because the recording would then carry that sound twice.

You can make your first recording without `ffmpeg`:

1. In the Sources dock, click + and add a Display capture source. On Linux, pick the display in
   the desktop's picker when it opens. On macOS, allow StudioCraft in System Settings › Privacy &
   Security › Screen Recording when the system asks, as
   [Troubleshooting](troubleshooting.md#capture) describes.
2. Open the Settings window: Edit › Preferences… on Windows and Linux, or StudioCraft › Settings…
   (⌘,) on macOS. Choose the Output tab, and set Video encoder to H.264 (built in). Keep Format at
   MKV. Click OK.
3. Click Record on the menu row. Click it again to stop.

StudioCraft saves the recording in the Recording folder set in Settings › Output. When that setting
is empty, it uses your Videos folder, then your home folder when the system has no Videos folder,
and then the folder StudioCraft was started in. It names each file `YYYY-MM-DD HH-MM-SS.<ext>`, for
example `2026-11-18 14-05-09.mkv`. File › Show Recordings opens the folder.

MP4 and MOV files, the Lossless quality level and the x264 encoder need `ffmpeg`. The built-in
hardware encoders record MKV files without it. [Encoders](features.md#encoders) lists which
encoder needs what.

## First stream

Streaming needs an `ffmpeg` that StudioCraft can run. The Flatpak cannot run an `ffmpeg` installed
on the system, so to stream on Linux, install another package, such as the `.AppImage`, `.deb` or
`.rpm`.

1. Install `ffmpeg` so that it is on the `PATH`, or set its location in Settings › Output ›
   ffmpeg program.
2. In Settings › Stream, enter the Server and the Stream key your streaming service gives you.
   Click OK.
3. Click Go Live on the menu row. Click it again to stop the stream.

A new profile streams at 6000 kbps video and 160 kbps audio with a keyframe every 2 s, using the
software x264 encoder. Settings › Stream changes these values, and
[MECHANICS.md](../MECHANICS.md#recording-and-streaming) lists every default.
[Troubleshooting](troubleshooting.md#recording-and-streaming) covers a stream that does not start
because `ffmpeg` was not found.

## Where StudioCraft keeps your files

StudioCraft keeps its settings, scene collections and profiles in its settings folder, a folder
named `studiocraft` in your system's settings location. File › Show Settings Folder opens it.
Scene collections are in its `scenes/` folder and profiles in its `profiles/` folder.

To keep everything in another folder, start the app with `--config-dir`:

```sh
studiocraft --config-dir /path/to/folder
```

`studiocraft --no-prefs` keeps everything in memory and reads and writes no settings.
[Command-line options](features.md#command-line-options) lists every option.
