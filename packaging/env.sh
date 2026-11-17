# shellcheck shell=bash
# Common setup for the packaging scripts, sourced with `. "$(dirname "$0")/../env.sh"`.
#
# Exports:
#   ROOT                     the workspace root
#   VERSION                  [workspace.package] version from Cargo.toml (STUDIOCRAFT_VERSION overrides it)
#   DIST                     where the release files go (default: $ROOT/dist/release)
#   STUDIOCRAFT_BUILD_SHA    the git commit, written into the macOS Info.plist
#   STUDIOCRAFT_BUILD_DATE   the UTC build date as YYYY-MM-DD, written into the AppStream release
#   CARGO_TARGET_DIR         cargo's target folder (default: $ROOT/target)

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
export ROOT

# `cargo xtask version` prints the same value; awk reads it without building xtask.
workspace_version() {
  awk '
    /^\[/ { in_pkg = ($0 == "[workspace.package]"); next }
    in_pkg && $1 == "version" { gsub(/[" ]/, "", $3); print $3; exit }
  ' "$ROOT/Cargo.toml"
  return
}

VERSION="${STUDIOCRAFT_VERSION:-$(workspace_version)}"
if [[ -z "$VERSION" ]]; then
  echo "error: could not read [workspace.package] version from $ROOT/Cargo.toml" >&2
  exit 1
fi
export VERSION

DIST="${DIST:-$ROOT/dist/release}"
mkdir -p "$DIST"
export DIST

if [[ -z "${STUDIOCRAFT_BUILD_SHA:-}" ]]; then
  STUDIOCRAFT_BUILD_SHA="$(git -C "$ROOT" rev-parse HEAD 2>/dev/null || true)"
fi
export STUDIOCRAFT_BUILD_SHA
export STUDIOCRAFT_BUILD_DATE="${STUDIOCRAFT_BUILD_DATE:-$(date -u +%Y-%m-%d)}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$ROOT/target}"

# Prints a GitHub Actions warning, or a plain warning on stderr outside Actions.
warn() {
  if [[ -n "${GITHUB_ACTIONS:-}" ]]; then echo "::warning::$*"; else echo "warning: $*" >&2; fi
  return
}

# Copies the license and readme files that exist into the folder $1.
copy_docs() {
  local dest="$1" f
  for f in README.md LICENSE-MIT LICENSE-APACHE NOTICE; do
    if [[ -f "$ROOT/$f" ]]; then cp "$ROOT/$f" "$dest/"; fi
  done
  return
}
