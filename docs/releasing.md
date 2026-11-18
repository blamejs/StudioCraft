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
3. An alpha version (`0.2.0-alpha.1`) is published by the workflow itself, as described in
   [Alpha releases](#alpha-releases). For any other version, open the draft on the Releases page
   when the run finishes, check its files, and publish it. A version with a pre-release part
   (`-rc.1`) is marked as a pre-release.

Another push to `release` before the draft is published rebuilds the same draft. Once it is
published, the next release needs a new version.

## Alpha releases

Alpha versions are `0.<minor>.<patch>-alpha.<N>`, for example `0.1.0-alpha.1`, then
`0.1.0-alpha.2`. Each one is tagged `v<version>` and published as a GitHub prerelease.

An alpha is cut only when at least 50 pull requests have merged into `main` since the previous
alpha tag. For the first alpha the count starts at the beginning of the repository.
`cargo xtask release-ready` does the count:

```sh
git fetch origin --tags
cargo xtask release-ready                  # counts up to origin/main
cargo xtask release-ready --ref <commit>   # counts up to another commit
```

It takes the newest `v*-alpha.*` tag that is an ancestor of the counted commit, lists the merged
pull requests with `gh pr list --state merged --base main`, and counts those whose merge commit is
in `<tag>..<commit>`. It prints the count and the pull requests, and exits 0 at 50 or more, 1 below
50 and 2 on an error (for example when `gh` is not signed in). `--notes <file>` also writes the
release notes from `packaging/release-notes.md`.

To cut an alpha:

1. Run `cargo xtask release-ready`. Below 50, do not cut the alpha.
2. Set the version through a pull request: `cargo xtask version set 0.1.0-alpha.2`.
3. After that pull request merges, fast-forward `main` into `release` and push it:

   ```sh
   git fetch origin
   git push origin origin/main:refs/heads/release
   ```

The push builds and signs every file and creates the draft `v<version>`. The workflow's `publish`
job then checks the draft and publishes it as a prerelease, which creates the tag at the built
commit. The draft is left unpublished, and the job fails, when any of these checks fails:

- The draft holds exactly these files: the Windows `x64` and `arm64` `.msi` and `-portable.zip`;
  the macOS `universal` `.dmg` and `studiocraft-cli` `.zip`; the Linux `x86_64` and `aarch64`
  `.AppImage`, `.AppImage.zsync`, `.deb`, `.rpm`, `.flatpak` and `.tar.gz`, each with its `.asc`
  signature; `SHA256SUMS.txt`, `SHA3-512SUMS.txt` and their `.asc` signatures; and
  `release-signing-key.asc`. When a Flatpak OCI image was pushed, its `.oci-manifest.json` and
  `.oci.sig` must both be there too. A missing architecture or format stops the release.
- `release-signing-key.asc` holds the key in `LINUX_SIGNING_KEY`, and every `.asc` signature
  verifies with `gpgv` against that public key. The `publish` job does not import the private key.
  The shared workflow's `linux` and `flatpak` jobs do: they import it in the job that builds the
  packages, and run the built AppImage and `packaging/linux/flatpak-bundle.sh` after the import.
- Every file matches `SHA256SUMS.txt` and `SHA3-512SUMS.txt`, and both list every file.
- `rpmkeys --checksig` reports `signatures OK` for both `.rpm` packages, and both `.deb` packages
  carry a `_gpgorigin` signature that verifies.
- On a Windows runner, `Get-AuthenticodeSignature` reports `Valid` for each `.msi` and for every
  `.exe` in each `-portable.zip`.
- `cargo xtask release-ready` counts at least 50 pull requests, and the notes it writes name no
  other product (the `cargo xtask brands` list). The command runs in the `release-ready` job, which
  has no environment and no secrets except a read-only `GITHUB_TOKEN`, which only the step that runs
  the built `xtask` receives. The `publish` job takes the
  notes from that job's artifact and does not check out or build any code.

An unsigned or missing file is a fault in the build to fix before the alpha is published. A manual
run of the workflow, and a push to `release` with a version that is not an alpha, leave a draft as
before.

The notes have this shape (the template is `packaging/release-notes.md`):

1. One line saying that the version is an alpha prerelease.
2. Which file to install on Windows, macOS and Linux, and which files are signed.
3. That streaming and encoders other than H.264 need a separately installed `ffmpeg`.
4. The commands that verify the checksum files and their signatures.
5. The number of pull requests merged since the previous alpha, the list of them, and the link to
   the full list of changes. The command writes each title as a code span on one line, so GitHub
   shows it as written, without formatting, links, HTML or mentions.

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
