# Roadmap

## Working now

- Scenes, sources, scene items with move, resize, rotate, lock and visibility, and undo.
- Built-in sources: color, image, text, media file, display capture, window capture, audio input
  and desktop audio.
- Filters: color correction, chroma key, gain, noise gate, compressor, limiter and a three-band
  equalizer.
- Transitions: cut, fade, fade through a color and swipe. Studio mode with quick transitions.
- Audio mixer with meters, faders and mute. The mixer measures the final mix's loudness
  (momentary, short-term, integrated, loudness range and true peak); the window does not show it
  yet.
- Recording to MKV, MP4 or MOV, and RTMP streaming, through an external ffmpeg.
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
  that is less. Dragging its top edge outside the header actions, or a divider, resizes it, and the
  sizes are kept until Docks › Reset Docks.
- Interface fonts through `craft-ui`'s font stack: Inter for text, Inter SemiBold for titles and
  JetBrains Mono for readouts, then the craft-fonts faces when `CRAFT_FONTS_DIR` is set, then
  egui's built-in faces. The menus' check and radio marks draw with or without `CRAFT_FONTS_DIR`.

## Next

1. **Capture on Linux.** Display and window capture through PipeWire portals, and audio devices
   through ALSA or PipeWire.
2. **Camera source.** Video capture devices on every system.
3. **More filters.** Crop and pad, scroll, sharpen, color key, image mask, and audio compressor and
   limiter.
4. **Replay buffer.** Keep the last N seconds in memory and save them on a shortcut.
5. **Audio monitoring.** Play a source to a chosen output device, and per-source audio offset.
6. **Docks.** Undock, rearrange and save dock layouts.
7. **Translations.** A `studiocraft.tsv` catalog for every other locale craft-i18n has, and
   right-to-left layout for Arabic and Hebrew.
8. **Multiple outputs.** Several streams at once, and a virtual camera.
9. **Web build.** The model and plugin crates already build for WebAssembly; a browser editor for
   scene collections is the first step.
