# Troubleshooting

This page lists problems you may meet while using StudioCraft, with their causes and fixes. Each
bold label is the symptom or the message the app shows.

## Recording and streaming

**"ffmpeg (set its location in Settings › Output) was not found" or "ffmpeg at {path} was not
found".** Streams, MP4 and MOV recordings, recordings at the Lossless quality level, and recordings with x264 or an
`ffmpeg` hardware encoder run in a separately installed `ffmpeg`, and StudioCraft could not find
it. Install `ffmpeg` so that it is on the `PATH`, or set its location in Settings › Output ›
ffmpeg program. Tools › Available Encoders lists the encoders the installed `ffmpeg` offers, and
says when `ffmpeg` was not found. To record without `ffmpeg`, set the Video encoder in Settings ›
Output to H.264 (built in) and record MKV files.

**An encoder in Settings is shown as "(not in this ffmpeg)".** The installed `ffmpeg` does not
offer that encoder. Pick another encoder, or install an `ffmpeg` build that has it.

**The Flatpak cannot find `ffmpeg`.** The Flatpak cannot run an `ffmpeg` installed on the system.
Record MKV files with H.264 (built in), or use another package to stream.

**"No hardware encoder passed its test; using {encoder}".** With the Hardware encoder set to
Automatic, StudioCraft tests each hardware encoder with one frame and uses x264 when none works.
Check that the graphics driver is installed, or pick a hardware encoder in Settings › Video ›
Hardware acceleration.

**"{encoder} stopped working; the next start tries another encoder".** A hardware encoder failed
while it ran. The next recording or stream tries another encoder.

## Capture

**"the desktop has no screen-cast portal".** On Linux, display and window capture need
`xdg-desktop-portal` with a backend for your desktop. Install them and change any of the source's
properties to try again. StudioCraft also tries again every 2 s.

**"the choice in the system picker was canceled" or "sharing stopped".** The desktop's picker was
closed, or sharing was stopped from the desktop's own controls. Change any of the source's
properties to open the picker again.

**"could not connect to the PipeWire sound server".** On Linux, audio capture and screen capture
go through PipeWire. Check that the PipeWire server is running. The source tries again on its own,
and the rest of the app keeps running.

## Cameras

**"the camera is in use by another camera source".** One camera shows in one camera source at a
time. Remove the other camera source, or pick another camera.

**"the camera is still closing".** A capture of the same camera is still ending. The source tries
again on its own.

**A camera shows as "(not connected)".** The saved camera is not plugged in. Plug it in, and the
source opens it again within 2 seconds. A camera that is missing, in use by another program,
unplugged or sending nothing for 5 seconds is reported in the Properties window and opened again
every 2 seconds.

**"camera access is not allowed in the system's privacy settings".** On macOS the system asks for
camera access the first time a camera is listed. Allow StudioCraft in the camera section of the
system's privacy settings.

## Graphics

**"No graphics hardware can be used; drawing in software, which is slower".** The window could not
open a device on a graphics adapter, and the app draws in software for this run. Check the
graphics driver, or pick another adapter in Settings › Video › Hardware acceleration.

**"The graphics device stopped working {count} times; drawing in software".** After a device loss
the app saves everything and starts again on a new device. After 3 losses in 10 minutes it starts
in software. A lost device also stops the recording, the stream and the replay buffer. Check the
graphics driver.

**"The graphics adapter {adapter} is not available; using the automatic choice".** The adapter
named in Settings › Video › Hardware acceleration is missing. The app uses Automatic instead.

## Updates

**"update checks are off in this session (--no-prefs, a scripted run or a test)".** A session
started with `--no-prefs`, a scripted run or a test does not check for updates. Start the app
normally and use Help › Check for Updates….

## Control channel

**"--control needs the session token in STUDIOCRAFT_CONTROL_TOKEN".** The app was started with
`--control` and no token. Set `STUDIOCRAFT_CONTROL_TOKEN` before you start it.

**"STUDIOCRAFT_CONTROL_TOKEN must be 64 hexadecimal characters".** The token has the wrong form.
Make a new one as [Connecting](control-protocol.md#connecting) shows.

**"set STUDIOCRAFT_CONTROL_TOKEN to the token the app was started with".**
`studiocraft-cli send` and `studiocraft-cli mcp --connect` read the token from the same variable
as the app. Set it in their shell to the token the app was started with.
