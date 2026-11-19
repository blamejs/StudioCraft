# Assets

Every picture, sound, font or video file in the repository is listed here with its author, source
and license. `cargo xtask assets` fails when a file is missing from this list.

The asset files are the app icon in `assets/app-icon`, the three interface faces in
`assets/fonts` and the screenshots in `docs/images`. The interface icons are drawn from shapes in
`crates/ui-egui/src/icons.rs`. The interface faces are built into every build
(`crates/ui-egui/src/fonts.rs`). When
`CRAFT_FONTS_DIR` is set at build time, the faces of a craft-fonts checkout outside this repository
are built in after them, for the scripts they lack, and egui's built-in fonts come last.

The JSON files in `crates/model/fixtures` (`collection-v1.json` and `profile-v1.json`) were written
by StudioCraft contributors for the save-format tests and are licensed MIT OR Apache-2.0.

The font files are the upstream releases, unmodified:

| File | Version | SHA-256 |
| --- | --- | --- |
| `assets/fonts/Inter-Regular.ttf` | Inter 4.1 (name table 4.001) | `40d692fce188e4471e2b3cba937be967878f631ad3ebbbdcd587687c7ebe0c82` |
| `assets/fonts/Inter-SemiBold.ttf` | Inter 4.1 (name table 4.001) | `78a843fade9d4612a5567302fb595b56976eb5fcebf4fea5a5912d638bafcde3` |
| `assets/fonts/JetBrainsMono-Regular.ttf` | JetBrains Mono 2.305 | `e6fd0d7e91550b3ed2b735d4312474362c4716edc4fc0577a0f61ed782d5aed1` |

The app icon is an engraving-style panda seated on a rounded purple tile. It is the project
owner's original drawing, a 1254 x 1254 raster whose tile is fully opaque in flat purple
(128, 66, 129); only the anti-aliased edge of the tile is partly transparent. `studiocraft-1024.png` is the drawing cropped to
its tile and scaled to 1024 x 1024; every other file is scaled from the drawing with Lanczos
resampling, as 8-bit RGBA with no text or time chunks. The PNG files and `studiocraft.ico` show
the tile filling the canvas. `studiocraft-macos-512.png` and `studiocraft.icns` follow Apple's
icon grid: the tile takes 824/1024 of the side, centered on a transparent canvas (412 px of
512 px). `studiocraft.ico` holds PNG images at 16, 24, 32, 48, 64, 128 and 256 px.
`studiocraft.icns` holds PNG images for every iconset size from 16 to 1024 px, @2x included.

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

Where each file is used:

- `hicolor/128x128`: the update prompt and the menu entry the updater writes beside an AppImage
  (`crates/ui-egui/src/app_icon.rs`, `UPDATE_APP` in `crates/ui-egui/src/lib.rs`).
- `hicolor/256x256`: the window icon on Windows, Linux and FreeBSD (title bar, taskbar and window
  switcher).
- `studiocraft-macos-512.png`: the window icon on macOS (Dock).
- `studiocraft.icns`: the macOS app bundle's icon (`StudioCraft.icns`).
- `studiocraft.ico`: the Windows installer's icon.
- `hicolor/<size>`: the Linux icon theme sizes for packages that install
  `share/icons/hicolor/<size>/apps/ai.storyteller.studiocraft.png`.
- `docs/images`: the screenshots in `README.md`.
