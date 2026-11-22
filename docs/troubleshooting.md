# Troubleshooting

This page lists problems you may meet while using StudioCraft, with their causes and fixes. Each
bold label is the symptom or the message the app shows.

## Recording and streaming

**"ffmpeg (set its location in Settings › Output) was not found" or "ffmpeg at {path} was not
found".** MP4 and MOV recordings, recordings at the Lossless quality level, and
recordings with x264 or an `ffmpeg` hardware encoder run in a separately installed `ffmpeg`, Media
file sources play files other than Ogg Vorbis at 8 to 192 kHz through it, and StudioCraft could not find it. Install `ffmpeg` so that it is on the `PATH`, or set its location
in Settings › Output › ffmpeg program. Tools › Available Encoders lists the encoders the installed
`ffmpeg` offers, and says when `ffmpeg` was not found. To record without `ffmpeg`, set the Video
encoder in Settings › Output to H.264 (built in) and record MKV files.

**An encoder in Settings is shown as "(not in this ffmpeg)".** The installed `ffmpeg` does not
offer that encoder. Pick another encoder, or install an `ffmpeg` build that has it.

**The Flatpak cannot find `ffmpeg`.** The Flatpak runtime has no `ffmpeg`, and the Flatpak sandbox
cannot run an `ffmpeg` installed on the system. Record MKV files with H.264 (built in). Streams do
not need `ffmpeg`.

**"No hardware encoder passed its test; using {encoder}".** With the Hardware encoder set to
Automatic, StudioCraft tests each hardware encoder with one frame and uses x264 when none works,
or H.264 (built in) for a stream.
Check that the graphics driver is installed, or pick a hardware encoder in Settings › Video ›
Hardware acceleration. The results are kept between sessions. After a fix that does not change
the driver or `ffmpeg`, choose Tools › Test Hardware Encoders Again, and the next recording or
stream tests the encoders again.

**"{encoder} stopped working; the next start tries another encoder".** A hardware encoder failed
while it ran. The next recording or stream tries another encoder.

The Streaming window shows why a destination is trying again or failed. Hold the pointer over the
status to see the error and each address that was tried.

**"this computer has no network route to the server addresses tried".** No network interface can
reach any of the server's addresses that were tried (at most 4). Check that the computer is connected to a network. On a network that
offers only IPv4 or only IPv6, check that the server has an address of that kind.

**"Waiting for the network".** This computer has no network route to the server: the network is
down, unplugged or not yet connected. It tries as soon as a route returns. A server given as an IP
address is not tried while the destination waits, and each longest wait of Settings › Stream spent
waiting counts as a failed try. A server given by name is still tried at the usual time. If the
computer is connected and the destination still waits, check the network adapter and any VPN that
changes the routes.

**"the server's name could not be looked up", "the server's name does not exist; check the server
address" or "the DNS server did not answer for the server's name".** The name in the server
address did not resolve. Check the spelling of the server address. When other sites open in a web
browser, the name may be wrong or the network's DNS server may block it. "the server's name does
not exist" appears only on Windows. "the DNS server did not answer" appears on every system when a
lookup gives no answer within 10 s, and on Windows also when the DNS server reports that it did not
answer.

**"earlier lookups of server names have not finished".** Earlier lookups are still waiting on the
system's resolver, so StudioCraft started no new one. Each destination keeps at most two lookups
outstanding and StudioCraft at most 32. The system's DNS settings or the network's DNS server are
not answering; the destination tries again as usual.

**"the server refused the connection".** The server answered, but nothing on it accepts streams on
that port. Check the port in the server address; `rtmp://` uses 1935 and `rtmps://` uses 443 when
the address names no port.

**"the server did not answer in time".** An address of the server that was tried did not answer
within 10 s, and none refused the connection. The server
may be down, or a firewall on this network may block the port. Try the service's `rtmps://` server
on port 443 if it has one.

**"the server's certificate is not from a trusted authority", "the server's certificate is for
another name", "the server's certificate has expired" or "the server's certificate was not
accepted".** The secure connection of an `rtmps://` server failed its certificate check. Check the
server address and the computer's clock. A network that inspects secure connections can also cause
this.

**"the server refused the stream; check the stream key".** The server took the connection and
refused to publish. Check the stream key, and that the stream is not already running from another
program with the same key.

**"the server refused the connection request; check the server address".** The server refused the
application named in the server address, the part after the host. Copy the server address from the
service again.

**"the server closed the connection before the stream started" or "the server did not finish
setting up the stream in time".** The server took the connection and ended it, or stopped answering,
before the stream started. The server may be busy or the address may point at a server that does
not take RTMP.

**"the connection was lost".** The connection ended after the stream started. StudioCraft tries
again on its own.

## Capture

**On macOS, a Display capture or Window capture source shows no picture.** A program can capture
the screen on macOS only after you allow it. Allow StudioCraft in System Settings › Privacy &
Security › Screen Recording, then quit StudioCraft and start it again.

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

**"Nothing can be drawn until StudioCraft starts again".** The graphics device was lost, and the
app did not start again on its own. This happens in a session started with `--no-prefs` or
`--control`, in a scripted run, when the window was already drawing in software, when the app asked
for software drawing on a system with no software adapter and lost the device 3 times in 10
minutes, or when the app could not save everything first. In the last case the status bar names
what was not saved. Fix that problem and save your work before you close the window: closing it
again discards what was not saved. Then close StudioCraft and start it again.

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
Make a new one as the [Command line](control-protocol.md#command-line) section shows.

**"set STUDIOCRAFT_CONTROL_TOKEN to the token the app was started with".**
`studiocraft-cli send` and `studiocraft-cli mcp --connect` read the token from the same variable
as the app. Set it in their shell to the token the app was started with.
