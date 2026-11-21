# Assets

This file lists every picture, sound, font or video file in the repository with its author, source
and license. `cargo xtask assets` fails when a file is missing from this list. The rows of the
author, source and license tables give each file's full path from the repository root in backticks.
The notes and the "Where each file is used" table use short names.

The asset files are the app icon in `assets/app-icon`, the three interface faces in
`assets/fonts`, the streaming service logos in `assets/platform-logos` and the screenshots in
`docs/images`. The interface icons are drawn from shapes in
`crates/ui-egui/src/icons.rs` and are not files.

## App icon

The app icon is an engraving-style panda seated on a rounded purple tile. It is the project owner's
original drawing, a 1254 × 1254 raster. The tile is fully opaque in flat purple (128, 66, 129), and
only the anti-aliased edge of the tile is partly transparent.

The files are made from the drawing as follows:

- `studiocraft-1024.png` is the drawing cropped to its tile and scaled to 1024 × 1024.
- Every other file is scaled from the drawing with Lanczos resampling, as 8-bit RGBA with no text
  or time chunks.
- The PNG files and `studiocraft.ico` show the tile filling the canvas.
- `studiocraft-macos-512.png` and `studiocraft.icns` follow Apple's icon grid: the tile takes
  824/1024 of the side, centered on a transparent canvas (412 px of 512 px).
- `studiocraft.ico` holds PNG images at 16, 24, 32, 48, 64, 128 and 256 px.
- `studiocraft.icns` holds PNG images for every iconset size from 16 to 1024 px, @2x included.

The app icon files have these authors, sources and licenses:

| File | Author | Source | License |
| --- | --- | --- | --- |
| `assets/app-icon/studiocraft-1024.png` | The StudioCraft project owner | Original drawing for StudioCraft | MIT OR Apache-2.0 |
| `assets/app-icon/hicolor/16x16/apps/ai.storyteller.studiocraft.png` | The StudioCraft project owner | Scaled from the original drawing | MIT OR Apache-2.0 |
| `assets/app-icon/hicolor/24x24/apps/ai.storyteller.studiocraft.png` | The StudioCraft project owner | Scaled from the original drawing | MIT OR Apache-2.0 |
| `assets/app-icon/hicolor/32x32/apps/ai.storyteller.studiocraft.png` | The StudioCraft project owner | Scaled from the original drawing | MIT OR Apache-2.0 |
| `assets/app-icon/hicolor/48x48/apps/ai.storyteller.studiocraft.png` | The StudioCraft project owner | Scaled from the original drawing | MIT OR Apache-2.0 |
| `assets/app-icon/hicolor/64x64/apps/ai.storyteller.studiocraft.png` | The StudioCraft project owner | Scaled from the original drawing | MIT OR Apache-2.0 |
| `assets/app-icon/hicolor/128x128/apps/ai.storyteller.studiocraft.png` | The StudioCraft project owner | Scaled from the original drawing | MIT OR Apache-2.0 |
| `assets/app-icon/hicolor/256x256/apps/ai.storyteller.studiocraft.png` | The StudioCraft project owner | Scaled from the original drawing | MIT OR Apache-2.0 |
| `assets/app-icon/hicolor/512x512/apps/ai.storyteller.studiocraft.png` | The StudioCraft project owner | Scaled from the original drawing | MIT OR Apache-2.0 |
| `assets/app-icon/studiocraft-macos-512.png` | The StudioCraft project owner | Scaled from the original drawing | MIT OR Apache-2.0 |
| `assets/app-icon/studiocraft.icns` | The StudioCraft project owner | Scaled from the original drawing | MIT OR Apache-2.0 |
| `assets/app-icon/studiocraft.ico` | The StudioCraft project owner | Scaled from the original drawing | MIT OR Apache-2.0 |

## Fonts

The interface faces are built into every build (`crates/ui-egui/src/fonts.rs`). When
`CRAFT_FONTS_DIR` is set at build time, the faces of a craft-fonts checkout outside this repository
are built in after them for the scripts they lack, and egui's built-in fonts come last.

The font files are the upstream releases, unmodified:

| File | Version | SHA-256 |
| --- | --- | --- |
| `assets/fonts/Inter-Regular.ttf` | Inter 4.1 (name table 4.001) | `40d692fce188e4471e2b3cba937be967878f631ad3ebbbdcd587687c7ebe0c82` |
| `assets/fonts/Inter-SemiBold.ttf` | Inter 4.1 (name table 4.001) | `78a843fade9d4612a5567302fb595b56976eb5fcebf4fea5a5912d638bafcde3` |
| `assets/fonts/JetBrainsMono-Regular.ttf` | JetBrains Mono 2.305 | `e6fd0d7e91550b3ed2b735d4312474362c4716edc4fc0577a0f61ed782d5aed1` |

The font files and their license texts have these authors, sources and licenses:

| File | Author | Source | License |
| --- | --- | --- | --- |
| `assets/fonts/Inter-Regular.ttf` | The Inter Project Authors | https://github.com/rsms/inter (release v4.1, `extras/ttf`) | OFL-1.1 (`assets/fonts/OFL-Inter.txt`) |
| `assets/fonts/Inter-SemiBold.ttf` | The Inter Project Authors | https://github.com/rsms/inter (release v4.1, `extras/ttf`) | OFL-1.1 (`assets/fonts/OFL-Inter.txt`) |
| `assets/fonts/JetBrainsMono-Regular.ttf` | The JetBrains Mono Project Authors | https://github.com/JetBrains/JetBrainsMono | OFL-1.1 (`assets/fonts/OFL-JetBrainsMono.txt`) |
| `assets/fonts/OFL-Inter.txt` | The Inter Project Authors | https://github.com/rsms/inter | The license text of the Inter files |
| `assets/fonts/OFL-JetBrainsMono.txt` | The JetBrains Mono Project Authors | https://github.com/JetBrains/JetBrainsMono | The license text of the JetBrains Mono file |
| `docs/images/main-window.png` | StudioCraft contributors | Screenshot of StudioCraft | MIT OR Apache-2.0 |
| `docs/images/studio-mode.png` | StudioCraft contributors | Screenshot of StudioCraft | MIT OR Apache-2.0 |
| `docs/images/transition.png` | StudioCraft contributors | Screenshot of StudioCraft | MIT OR Apache-2.0 |
| `docs/images/audio-mixer.png` | StudioCraft contributors | Screenshot of StudioCraft | MIT OR Apache-2.0 |
| `docs/images/settings.png` | StudioCraft contributors | Screenshot of StudioCraft | MIT OR Apache-2.0 |
| `docs/images/scopes.png` | StudioCraft contributors | Screenshot of StudioCraft | MIT OR Apache-2.0 |

## Streaming service logos

The Streaming window (View › Streaming) shows a service's logo beside a destination set up for that
service (AGENTS.md 1.2). Each file is the service's own download, byte for byte, downloaded on
2026-11-20. The services own the logos and their trademarks. StudioCraft does not license them, and
they are not covered by the project's license. The window draws each logo whole, at no less than the
minimum size its guidelines give, with their clear space, beside the words "Stream to" or, where
the guidelines allow no words beside it, on a row of its own.

| File | Owner | Source | Terms |
| --- | --- | --- | --- |
| `assets/platform-logos/twitch-glitch-purple.png` | Twitch Interactive, Inc. | `Twitch Brand/Twitch Logos/02. Glitch/01. Twitch Purple/glitch_flat_purple.png` in https://brand.twitch.com/uploads/Twitch-Brand.zip, from https://brand.twitch.com/ | https://legal.twitch.com/legal/trademark/ and the logo usage guidelines in the same download |
| `assets/platform-logos/youtube-icon-red.png` | Google LLC | `YouTube_Icon/Digital/01 Red/yt_icon_red_digital.png` in the YouTube icon download on https://brand.youtube/youtube-icon | https://brand.youtube/youtube-icon and https://developers.google.com/youtube/terms/branding-guidelines |
| `assets/platform-logos/facebook-logo-primary.png` | Meta Platforms, Inc. | `Facebook Brand Asset Pack/Logo/Primary Logo/Facebook_Logo_Primary.png` in the logo pack on https://www.meta.com/brand/resources/facebook/logo/ | https://www.meta.com/brand/resources/facebook/logo/ |

The logo files have these SHA-256 hashes:

| File | SHA-256 |
| --- | --- |
| `assets/platform-logos/twitch-glitch-purple.png` | `0a7bc78f69becd1d6a082b4740d8cc6c096db99a6f3fa586951a2126007434b4` |
| `assets/platform-logos/youtube-icon-red.png` | `1027b1b0517727adb9697155a270744381c3ce9b047b1c8bd8a9389dc7d07a83` |
| `assets/platform-logos/facebook-logo-primary.png` | `2adfd474d91fd20c51084309ed000c1ae6cc7f5f70af14d375930f5a71301308` |

The other services in the preset list do not publish a logo for this use, and the window shows their
names on a plain tile.

## Test fixtures

StudioCraft contributors wrote the JSON files in `crates/model/fixtures` for the save-format tests,
and they are licensed MIT OR Apache-2.0. The files are `collection-v1.json` to
`collection-v4.json` and `profile-v1.json` to `profile-v8.json`.

## Where each file is used

| File | Use |
| --- | --- |
| `hicolor/128x128` | The update prompt and the menu entry the updater writes beside an AppImage (`crates/ui-egui/src/app_icon.rs`, `UPDATE_APP` in `crates/ui-egui/src/lib.rs`). |
| `hicolor/256x256` | The window icon on Windows, Linux and FreeBSD (title bar, taskbar and window switcher). |
| `studiocraft-macos-512.png` | The window icon on macOS (Dock). |
| `studiocraft.icns` | The macOS app bundle's icon (`StudioCraft.icns`). |
| `studiocraft.ico` | The Windows installer's icon. |
| `assets/platform-logos` | The destination rows and details in the Streaming window (`crates/ui-egui/src/logos.rs`). |
| `hicolor/<size>` | The Linux icon theme sizes for packages that install `share/icons/hicolor/<size>/apps/ai.storyteller.studiocraft.png`. |
| `docs/images` | The screenshots in `README.md`. |
