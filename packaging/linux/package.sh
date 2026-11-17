#!/usr/bin/env bash
# Builds StudioCraft for Linux and writes, for <arch> x86_64 or aarch64:
#
#   $DIST/studiocraft-<version>-linux-<arch>.AppImage        runs on distributions with the build host's glibc or newer
#   $DIST/studiocraft-<version>-linux-<arch>.AppImage.zsync  delta updates (written when zsyncmake is installed)
#   $DIST/studiocraft-<version>-linux-<arch>.deb             Debian, Ubuntu and their derivatives
#   $DIST/studiocraft-<version>-linux-<arch>.rpm             Fedora, openSUSE, RHEL
#   $DIST/studiocraft-<version>-linux-<arch>.tar.gz          a bin/ and share/ tree
#
# Usage: packaging/linux/package.sh [--skip-build] [--formats "appimage deb rpm tar"]
#
# Needs cargo, the development packages in BUILD_PACKAGES below, nfpm for the deb and rpm
# (https://nfpm.goreleaser.com), and curl to download appimagetool when it is not on PATH.
# desktop-file-validate and appstreamcli check the desktop and AppStream files when installed.
# In GitHub Actions on Ubuntu, the script installs BUILD_PACKAGES with apt-get.
set -euo pipefail
# shellcheck source=../env.sh
. "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
HERE="$ROOT/packaging/linux"
APP_ID=ai.storyteller.studiocraft

SKIP_BUILD=0
FORMATS="appimage deb rpm tar"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --skip-build) SKIP_BUILD=1; shift ;;
    --formats) FORMATS="$2"; shift 2 ;;
    -h | --help) sed -n '2,17p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

ARCH="$(uname -m)"
case "$ARCH" in
  x86_64) DEB_ARCH=amd64 ;;
  aarch64 | arm64) ARCH=aarch64; DEB_ARCH=arm64 ;;
  *) echo "unsupported architecture $ARCH" >&2; exit 2 ;;
esac
export STUDIOCRAFT_MAINTAINER="${STUDIOCRAFT_MAINTAINER:-StudioCraft maintainers <https://github.com/blamejs/StudioCraft>}"
BASENAME="studiocraft-$VERSION-linux-$ARCH"

echo "==> StudioCraft $VERSION for Linux $ARCH ($FORMATS)"

# Display and window capture link PipeWire, GBM and EGL, and their bindings are generated with
# libclang. The CI gate installs the same list (.github/workflows/ci.yml).
BUILD_PACKAGES=(
  pkg-config libxkbcommon-dev libwayland-dev libx11-dev libxcb1-dev libxcursor-dev libxrandr-dev
  libxi-dev libfontconfig1-dev libfreetype-dev libasound2-dev libpipewire-0.3-dev libclang-dev
  libgbm-dev libegl-dev
)
if [[ "$SKIP_BUILD" = 0 ]] && [[ -n "${GITHUB_ACTIONS:-}" ]] && command -v apt-get >/dev/null; then
  sudo apt-get install -y --no-install-recommends "${BUILD_PACKAGES[@]}"
fi

if [[ "$SKIP_BUILD" = 0 ]]; then
  # shellcheck source=pipewire-headers.sh
  . "$HERE/pipewire-headers.sh"
  (cd "$ROOT" && cargo build --release --locked -p studiocraft -p studiocraft-cli --bins)
fi
BIN="$CARGO_TARGET_DIR/release"
WORK="$CARGO_TARGET_DIR/linux-package"
STAGE="$WORK/root"
rm -rf "$WORK"

# ---- the file tree every format packages ------------------------------------------------------
install -Dm755 "$BIN/studiocraft" "$STAGE/usr/bin/studiocraft"
install -Dm755 "$BIN/studiocraft-cli" "$STAGE/usr/bin/studiocraft-cli"
strip "$STAGE/usr/bin/studiocraft" "$STAGE/usr/bin/studiocraft-cli" 2>/dev/null || true
install -Dm644 "$HERE/$APP_ID.desktop" "$STAGE/usr/share/applications/$APP_ID.desktop"
mkdir -p "$STAGE/usr/share/metainfo"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@DATE@/$STUDIOCRAFT_BUILD_DATE/g" \
  "$HERE/$APP_ID.metainfo.xml.in" >"$STAGE/usr/share/metainfo/$APP_ID.metainfo.xml"
mkdir -p "$STAGE/usr/share/icons"
cp -R "$ROOT/assets/app-icon/hicolor" "$STAGE/usr/share/icons/"
install -Dm644 "$ROOT/assets/app-icon/studiocraft.svg" "$STAGE/usr/share/icons/hicolor/scalable/apps/$APP_ID.svg"
mkdir -p "$STAGE/usr/share/doc/studiocraft"
copy_docs "$STAGE/usr/share/doc/studiocraft"

if command -v desktop-file-validate >/dev/null; then
  desktop-file-validate "$STAGE/usr/share/applications/$APP_ID.desktop"
fi
if command -v appstreamcli >/dev/null; then
  appstreamcli validate --no-net --explain "$STAGE/usr/share/metainfo/$APP_ID.metainfo.xml"
fi

has() {
  case " $FORMATS " in *" $1 "*) return 0 ;; *) return 1 ;; esac
}

# ---- .tar.gz ----------------------------------------------------------------------------------
if has tar; then
  mkdir -p "$WORK/tar"
  cp -R "$STAGE/usr" "$WORK/tar/$BASENAME"
  tar -C "$WORK/tar" -czf "$DIST/$BASENAME.tar.gz" "$BASENAME"
  echo "wrote $DIST/$BASENAME.tar.gz"
fi

# ---- .deb and .rpm ----------------------------------------------------------------------------
if has deb || has rpm; then
  command -v nfpm >/dev/null || { echo "error: nfpm not found (https://nfpm.goreleaser.com/install/)" >&2; exit 1; }
  export NFPM_ARCH="$DEB_ARCH"
  # nfpm expands environment variables in fields such as `version`, but not in `contents[].src`.
  sed "s|\${STAGE}|$STAGE|g" "$HERE/nfpm.yaml" >"$WORK/nfpm.yaml"
  for fmt in deb rpm; do
    if has "$fmt"; then (cd "$ROOT" && nfpm package -f "$WORK/nfpm.yaml" -p "$fmt" -t "$DIST/$BASENAME.$fmt"); fi
  done
fi

# ---- AppImage ---------------------------------------------------------------------------------
if has appimage; then
  APPIMAGETOOL_VERSION=1.9.1
  case "$ARCH" in
    x86_64) APPIMAGETOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0 ;;
    aarch64) APPIMAGETOOL_SHA256=f0837e7448a0c1e4e650a93bb3e85802546e60654ef287576f46c71c126a9158 ;;
  esac
  # A pinned runtime; without --runtime-file appimagetool downloads the latest one.
  APPIMAGE_RUNTIME_VERSION=20251108
  case "$ARCH" in
    x86_64) APPIMAGE_RUNTIME_SHA256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d ;;
    aarch64) APPIMAGE_RUNTIME_SHA256=00cbdfcf917cc6c0ff6d3347d59e0ca1f7f45a6df1a428a0d6d8a78664d87444 ;;
  esac
  APPDIR="$WORK/StudioCraft.AppDir"
  cp -R "$STAGE" "$APPDIR"
  ln -s usr/bin/studiocraft "$APPDIR/AppRun"
  cp "$HERE/$APP_ID.desktop" "$APPDIR/$APP_ID.desktop"
  cp "$ROOT/assets/app-icon/hicolor/256x256/apps/$APP_ID.png" "$APPDIR/$APP_ID.png"
  ln -s "$APP_ID.png" "$APPDIR/.DirIcon"

  verify_sha256() {
    printf '%s  %s\n' "$1" "$2" | sha256sum -c -
  }
  TOOL="${APPIMAGETOOL:-$(command -v appimagetool || true)}"
  if [[ -z "$TOOL" ]]; then
    TOOL="$CARGO_TARGET_DIR/appimagetool-$APPIMAGETOOL_VERSION-$ARCH.AppImage"
    if [[ ! -x "$TOOL" ]]; then
      curl --proto '=https' -fsSL -o "$TOOL.download" \
        "https://github.com/AppImage/appimagetool/releases/download/$APPIMAGETOOL_VERSION/appimagetool-$ARCH.AppImage"
      verify_sha256 "$APPIMAGETOOL_SHA256" "$TOOL.download"
      chmod 755 "$TOOL.download"
      mv "$TOOL.download" "$TOOL"
    fi
    verify_sha256 "$APPIMAGETOOL_SHA256" "$TOOL"
  fi
  RUNTIME="$CARGO_TARGET_DIR/appimage-runtime-$APPIMAGE_RUNTIME_VERSION-$ARCH"
  if [[ ! -f "$RUNTIME" ]]; then
    curl --proto '=https' -fsSL -o "$RUNTIME.download" \
      "https://github.com/AppImage/type2-runtime/releases/download/$APPIMAGE_RUNTIME_VERSION/runtime-$ARCH"
    verify_sha256 "$APPIMAGE_RUNTIME_SHA256" "$RUNTIME.download"
    mv "$RUNTIME.download" "$RUNTIME"
  fi
  verify_sha256 "$APPIMAGE_RUNTIME_SHA256" "$RUNTIME"
  # appimagetool runs inside $DIST below, so every path it gets is absolute.
  OUT="$(cd "$DIST" && pwd)/$BASENAME.AppImage"
  APPDIR="$(cd "$APPDIR" && pwd)"
  TOOL="$(cd "$(dirname "$TOOL")" && pwd)/$(basename "$TOOL")"
  RUNTIME="$(cd "$(dirname "$RUNTIME")" && pwd)/$(basename "$RUNTIME")"
  rm -f "$OUT.zsync"
  # AppImage update tools read this to fetch the .zsync of the newest published release. A fork
  # points at its own releases through GITHUB_REPOSITORY.
  REPO="${GITHUB_REPOSITORY:-blamejs/StudioCraft}"
  UPDATE_INFO="gh-releases-zsync|${REPO%%/*}|${REPO#*/}|latest|studiocraft-*-linux-$ARCH.AppImage.zsync"
  # APPIMAGE_EXTRACT_AND_RUN lets appimagetool run without FUSE. With zsyncmake installed it also
  # writes the .zsync into its working folder.
  (cd "$DIST" && ARCH="$ARCH" APPIMAGE_EXTRACT_AND_RUN=1 "$TOOL" --no-appstream --runtime-file "$RUNTIME" -u "$UPDATE_INFO" "$APPDIR" "$OUT")
  echo "wrote $OUT"
  if [[ -s "$OUT.zsync" ]]; then
    echo "wrote $OUT.zsync"
  else
    warn "zsyncmake is not installed, so $OUT.zsync was not written"
  fi
fi

"$STAGE/usr/bin/studiocraft-cli" --version
echo "==> done"
ls -lh "$DIST"
