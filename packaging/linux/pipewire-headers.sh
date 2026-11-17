# shellcheck shell=bash
# Puts PipeWire 1.0.5's SPA headers ahead of the system's on PKG_CONFIG_PATH when the system's
# PipeWire is older than 1.0. Sourced with `. packaging/linux/pipewire-headers.sh` before a
# cargo build.
#
# The pipewire crate's SPA bindings need the SPA headers of PipeWire 1.0 or later. Ubuntu 22.04
# ships PipeWire 0.3.48. SPA is headers only, so the build compiles against the 1.0.5 headers and
# still links the system's libpipewire-0.3. The source archive is checked against its SHA-256.
#
# Needs pkg-config, curl, sha256sum and tar with bzip2. The headers go to
# $CARGO_TARGET_DIR/pipewire-headers (default: target/pipewire-headers).

pipewire_headers() {
  local version=1.0.5
  local sha256=d5e5f3d0b8460e5711c1571c500156fd61768ba55a062eaeb94356abdb955a56
  local url="https://archive.ubuntu.com/ubuntu/pool/main/p/pipewire/pipewire_${version}.orig.tar.bz2"
  local root
  root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
  local dir="${CARGO_TARGET_DIR:-$root/target}/pipewire-headers/$version"
  # Build scripts run in their own package directories, so the paths must be absolute. A
  # relative CARGO_TARGET_DIR is relative to the current directory, as cargo reads it.
  if [[ "$dir" != /* ]]; then dir="$PWD/$dir"; fi

  if ! command -v pkg-config >/dev/null || ! pkg-config --exists libpipewire-0.3; then
    return 0
  fi
  if pkg-config --atleast-version=1.0.0 libpipewire-0.3; then
    return 0
  fi

  if [[ ! -f "$dir/pkgconfig/libspa-0.2.pc" ]]; then
    echo "==> PipeWire $(pkg-config --modversion libpipewire-0.3) is older than 1.0: using the SPA headers of PipeWire $version"
    # Each step returns on failure: a caller that sources this file in a condition runs it
    # without errexit.
    rm -rf "$dir" || return 1
    mkdir -p "$dir/pkgconfig" || return 1
    curl --proto '=https' --tlsv1.2 --retry 5 --location --silent --show-error --fail \
      -o "$dir/pipewire.tar.bz2" "$url" || return 1
    if ! echo "$sha256  $dir/pipewire.tar.bz2" | sha256sum --check --quiet; then
      echo "error: $url does not match SHA-256 $sha256" >&2
      rm -rf "$dir"
      return 1
    fi
    tar -xjf "$dir/pipewire.tar.bz2" -C "$dir" --strip-components=2 "pipewire-$version/spa/include" || return 1
    rm "$dir/pipewire.tar.bz2" || return 1
    # libpipewire-0.3.pc requires libspa-0.2, so this file also sets the SPA include path of
    # every build that asks for libpipewire-0.3. It is written last, so its presence means the
    # headers are complete.
    cat >"$dir/pkgconfig/libspa-0.2.pc" <<EOF || return 1
includedir=$dir/include

Name: libspa
Description: Simple Plugin API (headers of PipeWire $version)
Version: 0.2
Cflags: -I\${includedir} -D_REENTRANT
EOF
  fi
  case ":${PKG_CONFIG_PATH:-}:" in
    *":$dir/pkgconfig:"*) ;;
    *) export PKG_CONFIG_PATH="$dir/pkgconfig${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}" ;;
  esac
  return 0
}
pipewire_headers
