StudioCraft {{version}} is an alpha prerelease. Expect missing features and bugs, and keep a copy of your scene collections before you open them in it.

## Install

| System | File |
| --- | --- |
| Windows x64 and Arm64 | `studiocraft-{{version}}-windows-<arch>.msi` installs StudioCraft. `studiocraft-{{version}}-windows-<arch>-portable.zip` runs without installing. |
| macOS (Apple silicon and Intel) | `studiocraft-{{version}}-macos-universal.dmg` holds the app. `studiocraft-cli-{{version}}-macos-universal.zip` holds the command-line tool. |
| Linux x86_64 and aarch64 | `.AppImage` runs without installing, and its `.AppImage.zsync` file lets an AppImage updater download only the changed parts. `.deb` and `.rpm` install a system package. `.flatpak` installs a Flatpak bundle. `.tar.gz` holds the files to unpack anywhere. |

The Windows files and the macOS app are signed, and the macOS app is notarized. The Linux `.rpm`, `.deb` and `.flatpak` packages are signed with the release key in `release-signing-key.asc`, and every Linux file has a detached `.asc` signature beside it.

Streaming, and recording with an encoder other than H.264, run a separately installed `ffmpeg`, which no package includes. The Flatpak cannot run an `ffmpeg` installed on the system. Every package, the Flatpak included, records MKV files with H.264 (built in) without `ffmpeg`.

## Verify the downloads

Put the downloaded files in one folder with `SHA256SUMS.txt`, `SHA3-512SUMS.txt`, their `.asc` signatures and `release-signing-key.asc`, then run:

```sh
gpg --import release-signing-key.asc
gpg --verify SHA256SUMS.txt.asc SHA256SUMS.txt
gpg --verify SHA3-512SUMS.txt.asc SHA3-512SUMS.txt
sha256sum --check --ignore-missing SHA256SUMS.txt
```

`openssl dgst -sha3-512 <file>` prints a file's SHA3-512 hash to compare with `SHA3-512SUMS.txt`.

## Changes

{{count}} pull requests merged since {{since}}:

{{pull_requests}}

Full list of changes: {{changes}}
