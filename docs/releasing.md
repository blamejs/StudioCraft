# Releasing

A StudioCraft release is a GitHub release in `blamejs/StudioCraft`. The in-app updater (Help › Check for Updates) and the Craft launcher read the release's
files by name, so every file follows one pattern.

## File names

```
studiocraft-<version>-<os>-<arch>.<ext>
```

- `<version>` is the workspace version from `Cargo.toml` (`[workspace.package] version`), without
  a leading `v`.
- `<os>` is `windows`, `macos`, `linux` or `freebsd`.
- `<arch>` is the architecture as the table below spells it. Neither `<os>` nor `<arch>` contains
  a hyphen.

| OS | Architectures | Packages |
| --- | --- | --- |
| Windows | `x64`, `x86`, `arm64` | `.msi` (installer), `-portable.zip` (the exe at the top level of the zip) |
| macOS | `universal` (or `x86_64` and `aarch64`) | `.dmg` |
| Linux | `x86_64`, `aarch64`, `riscv64` | `.AppImage`, `.deb`, `.rpm`, `.tar.gz` |
| FreeBSD | `x86_64`, `aarch64` | `.tar.gz` |

For version 0.1.0 the files are:

- `studiocraft-0.1.0-windows-x64.msi`
- `studiocraft-0.1.0-windows-x64-portable.zip`
- `studiocraft-0.1.0-macos-universal.dmg`
- `studiocraft-0.1.0-linux-x86_64.AppImage`
- `studiocraft-0.1.0-linux-x86_64.deb`
- `studiocraft-0.1.0-linux-x86_64.rpm`
- `studiocraft-0.1.0-linux-x86_64.tar.gz`
- `studiocraft-0.1.0-linux-aarch64.AppImage`
- `studiocraft-0.1.0-freebsd-x86_64.tar.gz`

The updater ignores other files in the release, such as `SHA256SUMS.txt`. A Windows portable zip
must end in `-portable.zip`; a plain `.zip` is not offered. The command-line tool may ship as
`studiocraft-cli-<version>-<os>-<arch>.zip`, which the updater does not install.

On each platform the updater tries the package kinds in this order:

- Windows: `.msi`, then `-portable.zip`. On x64 it falls back to an `x86` build, and on ARM64 to
  `x64`, then `x86`.
- macOS: `.dmg`, `universal` first. Apple silicon falls back to `aarch64`, then `x86_64`.
- Linux: `.AppImage`, then `.tar.gz`. The `.deb` and `.rpm` packages are offered as downloads.
- FreeBSD: `.tar.gz`.

`craft_update::pick_asset` does the matching. The test
`the_release_file_names_in_the_docs_are_ones_the_updater_picks` in
`crates/ui-egui/src/updates_ui.rs` checks every file name listed above against it.

## Ids the packages carry

| Where | Value |
| --- | --- |
| macOS bundle id (`CFBundleIdentifier`) | `ai.storyteller.studiocraft` |
| Flatpak id | `ai.storyteller.studiocraft` |
| Linux desktop file | `ai.storyteller.studiocraft.desktop` |
| Window app id (Wayland `app_id`, X11 `WM_CLASS`) | `ai.storyteller.studiocraft` |
| Settings folder name | `studiocraft` |

The window's app id must match the desktop file's name so that docks and task bars show the app's
icon. The constants are `BUNDLE_ID` and `APP_ID` in `crates/ui-egui/src/lib.rs`.

## App icon

The icon files are in `assets/app-icon`, and `ASSETS.md` lists each one. A Linux or FreeBSD
package installs each file in `assets/app-icon/hicolor/<size>/apps/` as
`share/icons/hicolor/<size>/apps/ai.storyteller.studiocraft.png`, for every size from 16 to 512.
The repository has no package scripts yet.
The app sets its window icon itself (`crates/ui-egui/src/app_icon.rs`), and the updater installs
the 128 px icon beside the menu entry it writes for an AppImage.

## Version lines

The launcher finds an installed copy's version by running it with `--version`:

```
$ studiocraft --version
StudioCraft 0.1.0
$ studiocraft-cli --version
studiocraft-cli 0.1.0
```

Each prints one line, the name and the workspace version separated by a space. The tests in
`tests/version.rs` and `apps/studiocraft-cli/tests/it/version.rs` check both lines.
