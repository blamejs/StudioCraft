#!/usr/bin/env bash
# CI only: imports the Developer ID certificate into a temporary keychain for codesign.
#
# Reads APPLE_CERTIFICATE (a base64 .p12), APPLE_CERTIFICATE_PASSWORD, KEYCHAIN_PASSWORD (random
# when unset) and RUNNER_TEMP (or TMPDIR). Writes MACOS_SIGN_IDENTITY (the identity's SHA-1) and
# MACOS_KEYCHAIN to $GITHUB_ENV in GitHub Actions, and prints them elsewhere.
#
# Without APPLE_CERTIFICATE it prints a warning and exits 0, and package.sh signs ad hoc.
set -euo pipefail
umask 077
# shellcheck source=../env.sh
. "$(dirname "${BASH_SOURCE[0]}")/../env.sh"

if [[ -z "${APPLE_CERTIFICATE:-}" ]]; then
  warn "APPLE_CERTIFICATE is not set, so the macOS build is signed ad hoc and not notarized"
  exit 0
fi

TMP="${RUNNER_TEMP:-${TMPDIR:-/tmp}}"
KEYCHAIN="$TMP/studiocraft-signing.keychain-db"
CERT="$TMP/studiocraft-signing.p12"
KC_PASS="${KEYCHAIN_PASSWORD:-$(openssl rand -hex 24)}"

trap 'rm -f "$CERT"' EXIT
printf '%s' "$APPLE_CERTIFICATE" | base64 --decode >"$CERT"
security create-keychain -p "$KC_PASS" "$KEYCHAIN"
security set-keychain-settings -lut 21600 "$KEYCHAIN"
security unlock-keychain -p "$KC_PASS" "$KEYCHAIN"
security import "$CERT" -P "${APPLE_CERTIFICATE_PASSWORD:-}" -T /usr/bin/codesign -T /usr/bin/security -t cert -f pkcs12 -k "$KEYCHAIN"
rm -f "$CERT"
# codesign may then use the key without a prompt.
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$KC_PASS" "$KEYCHAIN" >/dev/null
# The temporary keychain joins the search list, after which codesign and notarytool find it.
# shellcheck disable=SC2046
security list-keychains -d user -s "$KEYCHAIN" $(security list-keychains -d user | tr -d '"')

IDENTITY="$(security find-identity -v -p codesigning "$KEYCHAIN" | awk '/Developer ID Application/ { print $2; exit }')"
if [[ -z "$IDENTITY" ]]; then
  security find-identity -v -p codesigning "$KEYCHAIN" >&2
  echo "error: APPLE_CERTIFICATE holds no 'Developer ID Application' identity" >&2
  exit 1
fi
echo "Imported signing identity $IDENTITY"
if [[ -n "${GITHUB_ENV:-}" ]]; then
  { echo "MACOS_SIGN_IDENTITY=$IDENTITY"; echo "MACOS_KEYCHAIN=$KEYCHAIN"; } >>"$GITHUB_ENV"
else
  echo "MACOS_SIGN_IDENTITY=$IDENTITY"
  echo "MACOS_KEYCHAIN=$KEYCHAIN"
fi
