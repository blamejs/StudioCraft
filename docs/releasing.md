# Releasing

A StudioCraft release is a GitHub release in `blamejs/StudioCraft`. The in-app updater (Help › Check for Updates) and the Craft launcher read the release's
files by name, so every file follows one pattern.

## Making a release

`.github/workflows/release.yml` calls the suite's shared release workflow
(`storytold/craft-common-actions`, `.github/workflows/release.yml`, pinned to one commit). It builds
the Windows, macOS and Linux files, writes `SHA256SUMS.txt`, and creates or updates a draft
GitHub release named `StudioCraft v<version>`, tagged `v<version>`. The workflow's `checksums` job
then adds `SHA3-512SUMS.txt`. Each of the two files has one `<hex>  <file name>` line for every
file in the release on every platform, and both are computed after the Linux packages are signed.

1. Set the version on `main` through a pull request. It is `[workspace.package] version` in the
   root `Cargo.toml`:

   ```sh
   cargo xtask version                 # prints the current version, for example 0.1.0
   cargo xtask version set 0.2.0       # or 0.2.0-rc.1; changes Cargo.toml and Cargo.lock
   ```

2. Merge or fast-forward `main` into the `release` branch and push it. The workflow starts on the
   push.
3. When the run finishes, open the draft on the Releases page, check its files, and publish it.
   A version with a pre-release part (`-rc.1`) is marked as a pre-release.

Another push to `release` before the draft is published rebuilds the same draft. Once it is
published, the next release needs a new version.

The macOS and Windows jobs on the `release` branch stop unless their signing secrets are set in
the repository's `release` environment:

| Platform | Secrets |
| --- | --- |
| macOS (signing and notarization) | `APPLE_CERTIFICATE` (base64 `.p12`), `APPLE_CERTIFICATE_PASSWORD`, `KEYCHAIN_PASSWORD`, `APPLE_ID`, `APPLE_PASSWORD` (app-specific password), `APPLE_TEAM_ID` |
| Windows, with a certificate file | `WINDOWS_CERTIFICATE` (base64 `.pfx`), `WINDOWS_CERTIFICATE_PASSWORD` |
| Windows, with Azure Artifact Signing (formerly Trusted Signing) | `AZURE_TENANT_ID`, `AZURE_CLIENT_ID`, `AZURE_CLIENT_SECRET`, `AZURE_SIGNING_ENDPOINT`, `AZURE_SIGNING_ACCOUNT`, `AZURE_CERT_PROFILE` |
| Linux packages, `SHA256SUMS.txt` and `SHA3-512SUMS.txt` (optional) | `LINUX_SIGNING_KEY` (ASCII-armored OpenPGP secret key without a passphrase) |

`packaging/windows/sign.ps1` signs `studiocraft.exe`, `studiocraft-cli.exe` and the `.msi`. It
uses the certificate file when `WINDOWS_CERTIFICATE` is set, and otherwise Azure Artifact Signing
when all six Azure secrets are set. On macOS, `packaging/macos/package.sh` signs the app,
`studiocraft-cli` and the disk image, and notarizes them when the Apple secrets are set. On Linux,
`LINUX_SIGNING_KEY` signs the `.rpm` packages, the Flatpak bundle, `SHA256SUMS.txt` and
`SHA3-512SUMS.txt`. The two checksum files are signed only on the `release` branch, with detached
signatures `SHA256SUMS.txt.asc` and `SHA3-512SUMS.txt.asc`.

The release workflow builds Windows `x64` and `arm64`, macOS `universal`, and Linux `x86_64` and
`aarch64`. It does not build the FreeBSD, `riscv64` or WebAssembly files.

Actions › Release › Run workflow runs the same build from any branch. The `version` input
overrides `Cargo.toml` for that run, and `dry-run` builds and checks without a draft. A run that
is not on `release` leaves the files without signing secrets unsigned. Every night a dry run on
`main` builds everything except macOS.

The packaging scripts are in `packaging/`:

| Script | Writes |
| --- | --- |
| `packaging/windows/package.ps1 -Arch x64` | the `.msi` and `-portable.zip` |
| `packaging/macos/package.sh --arch universal` | the `.dmg` and the `studiocraft-cli` zip |
| `packaging/linux/package.sh` | the `.AppImage`, `.deb`, `.rpm` and `.tar.gz` |
| `packaging/linux/flatpak-bundle.sh` | the `.flatpak`, from the Linux `.tar.gz` |

Each writes to `dist/release/`. Streaming, and recording with an encoder other than H.264 (built
in), run a separately installed `ffmpeg`, which none of the packages include, and the Flatpak
sandbox cannot run one installed on the system. Every package, the Flatpak included, records MKV
files with H.264 (built in) without `ffmpeg`.

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

The updater ignores other files in the release, such as `SHA256SUMS.txt` and `SHA3-512SUMS.txt`. A Windows portable zip
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
The icon is a raster drawing, so the packages install no scalable icon. The macOS package copies
`studiocraft.icns` into the app bundle as `StudioCraft.icns`, and the Windows package uses
`studiocraft.ico` as the installer's icon.
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
