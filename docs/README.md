# Documentation

This page lists StudioCraft's documentation, grouped by what you want to do.

## Use the app

- [Getting started](getting-started.md) shows how to install a release, make a first recording and
  a first stream, and find the settings folder.
- [Features](features.md) describes every source, filter, transition, output, setting and window
  of the app.
- [Troubleshooting](troubleshooting.md) lists the messages the app shows when something fails, with
  their causes and fixes.
- [ROADMAP.md](../ROADMAP.md) lists what ships today and what comes next.

## Control and automate it

- [Control protocol](control-protocol.md) describes the control channel: the methods and their
  limits, and the command-line tool, with how to make the session token.
- [MCP server](mcp.md) shows how to register the Model Context Protocol (MCP) server with a client,
  and lists its tools, resources and prompts.

## Contribute

- [AGENTS.md](../AGENTS.md) holds the rules every change follows, and the checks that
  `cargo xtask ci` runs.
- [Architecture](architecture.md) describes the crates and their layers, the threads and how data
  moves between them, and the extension points.
- [Performance](performance.md) lists the time budgets, the measured baselines and how to measure
  them.
- [MECHANICS.md](../MECHANICS.md) lists every timing and tuning value, where it lives in the code,
  and where it came from.
- [ASSETS.md](../ASSETS.md) lists every picture, sound, font or video file in the repository
  with its author, source and license.

Each shared crate in `crates/` has a README that describes it for use in other apps:

- [craft-audio-dsp](../crates/craft-audio-dsp/README.md) holds audio effects, the loudness meter
  and resampling.
- [craft-color-transform](../crates/craft-color-transform/README.md) holds color spaces, white
  balance, tone curves and tone mapping.
- [craft-ease](../crates/craft-ease/README.md) holds easing curves, keyed tracks, motion paths and
  springs.
- [craft-flv](../crates/craft-flv/README.md) writes and reads FLV tags and AMF0 values.
- [craft-gpu-device](../crates/craft-gpu-device/README.md) chooses and opens a wgpu device and
  recovers after a device loss.
- [craft-gpu-yuv](../crates/craft-gpu-yuv/README.md) converts RGBA frames to NV12, P010 and I420.
- [craft-hwenc](../crates/craft-hwenc/README.md) tests hardware encoders and picks one
  automatically.
- [craft-hwenc-native](../crates/craft-hwenc-native/README.md) calls hardware encoders through
  their drivers.
- [craft-scopes](../crates/craft-scopes/README.md) computes waveforms, parades, histograms and
  vectorscopes.
- [craft-tsmux](../crates/craft-tsmux/README.md) writes MPEG-2 transport streams.

## Release

- [Releasing](releasing.md) describes how to build, sign and publish a release, and how the release
  files are named.
