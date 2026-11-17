# Roadmap

## Working now

- Scenes, sources, scene items with move, resize, rotate, lock and visibility, and undo.
- Built-in sources: color, image, text, media file, display capture, window capture, camera, audio
  input and desktop audio.
- Camera capture on Windows (Media Foundation), macOS (AVFoundation) and Linux (V4L2), with the
  camera and its format (size, frame rate and pixel encoding) chosen in the source's Properties.
- Audio input and desktop audio capture on Linux through the PipeWire sound server. The device
  lists show PipeWire's devices with the default first.
- Display and window capture on Linux through the desktop's screen-cast portal and PipeWire. The
  display or window is chosen in the desktop's picker when the source starts.
- Filters: color correction with white balance by temperature and tint, chroma key, color key,
  crop and pad, image mask, scroll, sharpen, gain, noise gate, compressor, limiter and a
  three-band equalizer.
- Transitions: cut, fade, fade through a color and swipe, with an Easing setting on the fades and
  the swipe. Studio mode with quick transitions.
- The View › Scopes window: a waveform, an RGB parade, a histogram, and the YUV and HLS
  vectorscopes of the program output.
- Audio mixer with meters, faders and mute, and a sync offset of 0 to 5000 ms for each source's
  audio (in the source's Properties window). The mixer measures the final mix's loudness
  (momentary, short-term, integrated, loudness range and true peak); the window does not show it
  yet.
- Audio monitoring on Windows and macOS: each source is off, monitor only (left out of recordings
  and streams) or monitor and output (in the source's Properties window), and monitored sources
  play on the device chosen in Settings › Audio.
- Recording to MKV with the built-in H.264 and AAC encoders, without ffmpeg (H.264 (built in) in
  Settings › Output, with a Speed, Balanced or Quality preset).
- The replay buffer: the built-in H.264 and AAC encoders keep the last 5 to 3600 seconds in memory,
  within a memory limit of 32 to 16384 MB (Settings › Output), and File › Save Replay or a
  shortcut writes them to an MKV file while the buffer keeps running.
- Recording to MKV, MP4 or MOV, Lossless recording, and RTMP streaming, through an external ffmpeg.
  The encoder's settings, such as the x264 and NVENC presets, are set in Settings › Output and
  Settings › Stream.
- Profiles, scene collections, rebindable shortcuts, the stats window, the control channel and the
  MCP server.
- MCP window tools when the server is connected to the app (`studiocraft-cli mcp --connect
  <port>`): inspect, screenshot, click, drag, keys, typing, menus, dialog answers and quit.
- Scene Collection › Import… and Export… for scene collection files.
- Credits in Help › About StudioCraft, with a choice of username, display name or real name, and
  the license texts.
- Interface translations from StudioCraft's own catalogs in `crates/ui-egui/i18n/locales`, merged
  with the shared `craft-i18n` catalogs when `CRAFT_I18N_DIR` is set, with a Language setting in
  Settings › General. English and German are built in.
- Help › Check for Updates…, a check at start (Settings › General › Check for updates at start, on
  by default) and finishing an update the app installed itself, through `craft-update`.
- The app icon in `assets/app-icon`: the window icon, the picture in the update prompt and the icon
  of the menu entry the updater writes beside an AppImage.
- A compact dark and light theme: 24-point controls and menu rows, 12-point body text, menus sized
  to their content, and a focus ring for keyboard navigation.
- The output controls on the menu row: an Edit and Studio switch, the transition picker and its
  duration, and Record and Go Live with their timers. A narrow window moves the transition picker
  behind an icon button and leaves out the buttons' labels.
- The dock strip: Scenes, Sources and Audio Mixer under 24-point headers that hold each dock's
  actions, separated by 1-point dividers. The strip is 200 points tall, or 30% of the window when
  that is less. Dragging its top edge outside the header actions, or a divider, resizes it, and a
  right-click on a dock's title moves the dock left or right, or undocks it into a window of its
  own. The sizes, the order and the undocked windows' places are kept until Docks › Reset Docks,
  and Docks › Save Dock Layout… keeps them under a name to put back later from Docks › Layouts.
- Interface fonts through `craft-ui`'s font stack: Inter for text, Inter SemiBold for titles and
  JetBrains Mono for readouts, then the craft-fonts faces when `CRAFT_FONTS_DIR` is set, then
  egui's built-in faces. The menus' check and radio marks draw with or without `CRAFT_FONTS_DIR`.

## Next

1. **Translations.** A `studiocraft.tsv` catalog for every other locale craft-i18n has, and
   right-to-left layout for Arabic and Hebrew.
2. **Multiple outputs.** Several streams at once, and a virtual camera.
3. **Web build.** The model and plugin crates already build for WebAssembly; a browser editor for
   scene collections is the first step.
