#!/usr/bin/env bash
# Builds, signs and, when the Apple credentials are set, notarizes StudioCraft for macOS:
#
#   $DIST/studiocraft-<version>-macos-<arch>.dmg       StudioCraft.app on a disk image with an Applications link
#   $DIST/studiocraft-cli-<version>-macos-<arch>.zip   the command-line tool
#
# Usage: packaging/macos/package.sh [--arch universal|aarch64|x86_64] [--skip-build]
#
# Signing (environment):
#   MACOS_SIGN_IDENTITY   the codesign identity (a name or SHA-1). The default "-" signs ad hoc,
#                         which Gatekeeper rejects on other Macs. In CI, import-cert.sh sets it.
#   MACOS_KEYCHAIN        the keychain that holds the identity (optional)
# Notarization needs a real identity and all three of APPLE_ID, APPLE_PASSWORD (an app-specific
# password) and APPLE_TEAM_ID. NOTARY_TIMEOUT_MINUTES (default 80) limits the wait for Apple.
set -euo pipefail
# shellcheck source=../env.sh
. "$(dirname "${BASH_SOURCE[0]}")/../env.sh"
HERE="$ROOT/packaging/macos"

ARCH=universal
SKIP_BUILD=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --arch) ARCH="$2"; shift 2 ;;
    --skip-build) SKIP_BUILD=1; shift ;;
    -h | --help) sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
case "$ARCH" in
  universal) TARGETS=(aarch64-apple-darwin x86_64-apple-darwin) ;;
  aarch64) TARGETS=(aarch64-apple-darwin) ;;
  x86_64) TARGETS=(x86_64-apple-darwin) ;;
  *) echo "unknown --arch $ARCH" >&2; exit 2 ;;
esac

# LSMinimumSystemVersion in Info.plist.in has the same value.
export MACOSX_DEPLOYMENT_TARGET=11.0
IDENTITY="${MACOS_SIGN_IDENTITY:--}"
SHORT_VERSION="${VERSION%%-*}"
WORK="$CARGO_TARGET_DIR/macos-package"
APP="$WORK/StudioCraft.app"
DMG="$DIST/studiocraft-$VERSION-macos-$ARCH.dmg"
CLI_ZIP="$DIST/studiocraft-cli-$VERSION-macos-$ARCH.zip"

# Every notarization in this run waits until the same deadline: NOTARY_TIMEOUT_MINUTES (default 80)
# after the script starts. The release job stops after 90 minutes, and that limit includes the build.
NOTARY_TIMEOUT_MINUTES="${NOTARY_TIMEOUT_MINUTES-80}"
if ! [[ "$NOTARY_TIMEOUT_MINUTES" =~ ^[1-9][0-9]{0,3}$ ]]; then
  echo "NOTARY_TIMEOUT_MINUTES must be a whole number of minutes from 1 to 9999, not '$NOTARY_TIMEOUT_MINUTES'" >&2
  exit 2
fi
NOTARY_DEADLINE=$(($(date +%s) + NOTARY_TIMEOUT_MINUTES * 60))
NOTARIZE=0
if [[ "$IDENTITY" = "-" ]]; then
  warn "macOS: no MACOS_SIGN_IDENTITY, so the app is signed ad hoc and not notarized"
elif [[ -n "${APPLE_ID:-}" ]] && [[ -n "${APPLE_PASSWORD:-}" ]] && [[ -n "${APPLE_TEAM_ID:-}" ]]; then
  NOTARIZE=1
else
  warn "macOS: APPLE_ID, APPLE_PASSWORD or APPLE_TEAM_ID is missing, so the app is signed but not notarized"
fi

echo "==> StudioCraft $VERSION for macOS ($ARCH), identity: $IDENTITY, notarize: $NOTARIZE"

if [[ "$SKIP_BUILD" = 0 ]]; then
  args=()
  for t in "${TARGETS[@]}"; do args+=(--target "$t"); done
  (cd "$ROOT" && cargo build --release --locked -p studiocraft -p studiocraft-cli "${args[@]}")
fi

rm -rf "$WORK"
mkdir -p "$WORK/bin"
for bin in studiocraft studiocraft-cli; do
  inputs=()
  for t in "${TARGETS[@]}"; do inputs+=("$CARGO_TARGET_DIR/$t/release/$bin"); done
  lipo -create -output "$WORK/bin/$bin" "${inputs[@]}"
  lipo -info "$WORK/bin/$bin"
done

# A real identity gets the hardened runtime's secure timestamp; an ad hoc signature cannot have one.
sign() {
  local ts=(--timestamp)
  [[ "$IDENTITY" = "-" ]] && ts=(--timestamp=none)
  local kc=()
  [[ -n "${MACOS_KEYCHAIN:-}" ]] && kc=(--keychain "$MACOS_KEYCHAIN")
  codesign --force --sign "$IDENTITY" ${kc[@]+"${kc[@]}"} "${ts[@]}" "$@"
}

# stapler fetches the ticket from Apple and the connection sometimes drops, so it is retried.
stapler_retry() {
  local action="$1" attempt
  for attempt in 1 2 3 4 5; do
    if xcrun stapler "$@"; then return 0; fi
    [[ "$attempt" = 5 ]] && { echo "stapler $action failed 5 times" >&2; return 1; }
    warn "stapler $action failed (attempt $attempt of 5); trying again in $((attempt * 15))s"
    sleep $((attempt * 15))
  done
}

notary() {
  xcrun notarytool "$@" --apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID"
}

# Prints one key of a JSON file, or nothing when the key is missing. plutil reads no data from a
# pipe, and it prints its errors on stdout.
json_value() {
  local value
  if value="$(plutil -extract "$2" raw -o - "$1" 2>/dev/null)"; then printf '%s' "$value"; fi
}

notarize() {
  local file="$1" name out err id status remaining
  name="$(basename "$file")"
  out="$WORK/notary-$name.json"
  err="$WORK/notary-$name.err"
  echo "==> notarizing $name"
  if ! notary submit "$file" --output-format json >"$out" 2>"$err"; then
    cat "$out" "$err" >&2
    echo "error: notarytool submit failed for $name" >&2
    exit 1
  fi
  id="$(json_value "$out" id)"
  if [[ -z "$id" ]]; then
    cat "$out" "$err" >&2
    echo "error: notarytool submit returned no submission id for $name" >&2
    exit 1
  fi
  echo "submission id: $id"
  # notarytool wait reports no status after a timeout or a dropped connection; notarytool info then
  # reads it. While the submission is in progress or the status is unknown, the wait starts again
  # until the deadline.
  while :; do
    remaining=$((NOTARY_DEADLINE - $(date +%s)))
    status=""
    if [[ "$remaining" -gt 0 ]]; then
      notary wait "$id" --timeout "${remaining}s" --output-format json >"$out" 2>"$err" || true
      cat "$out"
      echo
      status="$(json_value "$out" status)"
    fi
    if [[ -z "$status" ]]; then
      notary info "$id" --output-format json >"$out" 2>>"$err" || true
      cat "$out"
      echo
      status="$(json_value "$out" status)"
    fi
    if [[ -n "$status" ]] && [[ "$status" != "In Progress" ]]; then break; fi
    if [[ "$(date +%s)" -ge "$NOTARY_DEADLINE" ]]; then break; fi
    sleep 30
  done
  case "$status" in
    Accepted) ;;
    "In Progress")
      echo "error: notarization of $name (submission $id) was still in progress at the deadline;" \
        "check it with notarytool info $id and run the release again once it is accepted" >&2
      exit 1
      ;;
    *)
      cat "$err" >&2
      notary log "$id" || true
      echo "error: notarization of $name (submission $id) failed (status: ${status:-unknown})" >&2
      exit 1
      ;;
  esac
}

# Retries hdiutil, which sometimes fails on CI runners with "Resource busy".
hdiutil_retry() {
  local what="$1" attempt
  shift
  for attempt in 1 2 3 4 5; do
    if hdiutil "$@"; then return 0; fi
    [[ "$attempt" = 5 ]] && { echo "hdiutil $what failed 5 times" >&2; return 1; }
    warn "hdiutil $what failed (attempt $attempt of 5); trying again in $((attempt * 10))s"
    sleep $((attempt * 10))
  done
}

# ---- StudioCraft.app --------------------------------------------------------------------------
echo "==> assembling $APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$WORK/bin/studiocraft" "$APP/Contents/MacOS/StudioCraft"
# The icon set is made from the 512 px macOS icon.
ICONSET="$WORK/StudioCraft.iconset"
mkdir -p "$ICONSET"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$ROOT/assets/app-icon/studiocraft-macos-512.png" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  if [[ "$double" -le 512 ]]; then
    sips -z "$double" "$double" "$ROOT/assets/app-icon/studiocraft-macos-512.png" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
  fi
done
iconutil -c icns -o "$APP/Contents/Resources/StudioCraft.icns" "$ICONSET"
sed -e "s/@VERSION@/$VERSION/g" -e "s/@SHORT_VERSION@/$SHORT_VERSION/g" \
  -e "s/@BUILD_SHA@/${STUDIOCRAFT_BUILD_SHA:-unknown}/g" \
  "$HERE/Info.plist.in" >"$APP/Contents/Info.plist"
plutil -lint "$APP/Contents/Info.plist"
printf 'APPL????' >"$APP/Contents/PkgInfo"

# `codesign --verify --strict` rejects extended attributes such as quarantine and FinderInfo.
xattr -cr "$APP"
sign --options runtime --entitlements "$HERE/entitlements.plist" "$APP/Contents/MacOS/StudioCraft"
sign --options runtime --entitlements "$HERE/entitlements.plist" "$APP"
codesign --verify --strict --deep --verbose=2 "$APP"

if [[ "$NOTARIZE" = 1 ]]; then
  ditto -c -k --keepParent "$APP" "$WORK/StudioCraft-notarize.zip"
  notarize "$WORK/StudioCraft-notarize.zip"
  stapler_retry staple "$APP"
  stapler_retry validate "$APP"
  spctl --assess --type execute -vvv "$APP"
fi

# ---- DMG --------------------------------------------------------------------------------------
echo "==> building $DMG"
STAGE="$WORK/dmg"
mkdir -p "$STAGE"
ditto "$APP" "$STAGE/StudioCraft.app"
ln -s /Applications "$STAGE/Applications"
rm -f "$DMG"
hdiutil_retry create create -srcfolder "$STAGE" -volname "StudioCraft" -fs HFS+ -format UDZO -imagekey zlib-level=9 -ov "$DMG"
sign "$DMG"
codesign --verify --strict --verbose=2 "$DMG"
# The app inside the image must still pass strict verification.
MOUNT="$WORK/dmg-check"
rm -rf "$MOUNT"
mkdir -p "$MOUNT"
hdiutil_retry attach attach -nobrowse -readonly -noautoopen -mountpoint "$MOUNT" "$DMG" >/dev/null
dmg_ok=1
codesign --verify --strict --deep --verbose=2 "$MOUNT/StudioCraft.app" || dmg_ok=0
hdiutil_retry detach detach "$MOUNT" >/dev/null || dmg_ok=0
if [[ "$dmg_ok" != 1 ]]; then
  echo "error: StudioCraft.app in $(basename "$DMG") fails strict verification" >&2
  exit 1
fi
if [[ "$NOTARIZE" = 1 ]]; then
  notarize "$DMG"
  stapler_retry staple "$DMG"
  stapler_retry validate "$DMG"
  spctl --assess --type open --context context:primary-signature -vvv "$DMG"
fi

# ---- command-line tool ------------------------------------------------------------------------
echo "==> building $CLI_ZIP"
CLI_DIR="$WORK/studiocraft-cli-$VERSION-macos-$ARCH"
mkdir -p "$CLI_DIR"
cp "$WORK/bin/studiocraft-cli" "$CLI_DIR/"
copy_docs "$CLI_DIR"
sign --options runtime "$CLI_DIR/studiocraft-cli"
codesign --verify --strict --verbose=2 "$CLI_DIR/studiocraft-cli"
rm -f "$CLI_ZIP"
ditto -c -k --keepParent "$CLI_DIR" "$CLI_ZIP"
# A bare executable cannot hold a stapled ticket; Gatekeeper looks the notarization up online.
if [[ "$NOTARIZE" = 1 ]]; then notarize "$CLI_ZIP"; fi

"$WORK/bin/studiocraft-cli" --version
echo "==> done"
ls -lh "$DMG" "$CLI_ZIP"
