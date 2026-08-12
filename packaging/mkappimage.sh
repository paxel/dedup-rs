#!/usr/bin/env bash
# Build an AppImage for the `dedup` binary.
#
# Produces a snapshot by default (version suffixed with a sortable
# ~snapshot<timestamp>.<git-sha>, matching mkdeb.sh) or a clean release build
# with --release.
#
# The AppDir bundles only the static `dedup` binary, the desktop entry and the
# icon — the GUI dlopens its OpenGL / X11 / Wayland libraries from the host at
# runtime, exactly like the .deb (which lists them as Recommends), so nothing
# else needs bundling. AppRun is a symlink to the binary; `dedup` without
# arguments launches the GUI.
#
# `appimagetool` is taken from $APPIMAGETOOL, then $PATH, and otherwise
# downloaded once into target/ (the official continuous build for the host
# arch). It is always run with --appimage-extract-and-run so no FUSE is needed
# (CI runners don't have it).
#
# Usage:
#   packaging/mkappimage.sh [--snapshot|--release] [--target <triple>]
#                           [--version <ver>] [--out <dir>] [--no-build]
#
# Examples:
#   packaging/mkappimage.sh                  # snapshot AppImage, host arch
#   packaging/mkappimage.sh --release --out dist --no-build
set -euo pipefail

MODE=snapshot
TARGET=""
OUTDIR="dist"
VERSION=""
DO_BUILD=1

while [ $# -gt 0 ]; do
  case "$1" in
    --snapshot) MODE=snapshot ;;
    --release)  MODE=release ;;
    --target)   TARGET="$2"; shift ;;
    --version)  VERSION="$2"; shift ;;
    --out)      OUTDIR="$2"; shift ;;
    --no-build) DO_BUILD=0 ;;
    -h|--help)  sed -n '2,25p' "$0"; exit 0 ;;
    *) echo "mkappimage: unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

# --- version -----------------------------------------------------------------
base_ver=$(grep -m1 '^version' crates/dedup-cli/Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')
if [ -n "$VERSION" ]; then
  ver="$VERSION"
elif [ "$MODE" = release ]; then
  ver="$base_ver"
else
  sha=$(git rev-parse --short=8 HEAD 2>/dev/null || echo nogit)
  ts=$(date -u +%Y%m%d%H%M%S)
  ver="${base_ver}~snapshot${ts}.${sha}"
fi

# --- build -------------------------------------------------------------------
build_flags=(--release -p dedup-rs-cli --locked)
[ -n "$TARGET" ] && build_flags+=(--target "$TARGET")
if [ "$DO_BUILD" -eq 1 ]; then
  echo "mkappimage: building dedup ($MODE, version $ver${TARGET:+, target $TARGET})…"
  cargo build "${build_flags[@]}"
fi
bin="target/${TARGET:+$TARGET/}release/dedup"
[ -x "$bin" ] || { echo "mkappimage: binary not found at $bin (drop --no-build?)" >&2; exit 1; }

# --- architecture ------------------------------------------------------------
# appimagetool wants x86_64 / aarch64; the artifact name keeps the repo's
# x86_64 / arm64 convention (matching the tarball and the .deb).
if [ -n "$TARGET" ]; then
  case "$TARGET" in
    x86_64-*)  arch=x86_64 ;;
    aarch64-*) arch=aarch64 ;;
    *) echo "mkappimage: cannot map target '$TARGET' to an AppImage arch" >&2; exit 1 ;;
  esac
else
  arch=$(uname -m)
  case "$arch" in
    x86_64|aarch64) ;;
    arm64) arch=aarch64 ;;
    *) echo "mkappimage: unsupported host arch '$arch'" >&2; exit 1 ;;
  esac
fi
name_arch=$arch
[ "$arch" = aarch64 ] && name_arch=arm64

# --- appimagetool ------------------------------------------------------------
tool="${APPIMAGETOOL:-}"
if [ -z "$tool" ] && command -v appimagetool >/dev/null 2>&1; then
  tool=$(command -v appimagetool)
fi
if [ -z "$tool" ]; then
  tool="target/appimagetool-$arch.AppImage"
  if [ ! -x "$tool" ]; then
    url="https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-$arch.AppImage"
    echo "mkappimage: downloading appimagetool ($url)…"
    curl -fsSL --retry 4 --retry-delay 2 -o "$tool" "$url"
    chmod +x "$tool"
  fi
fi

# --- stage the AppDir --------------------------------------------------------
appdir=$(mktemp -d)
trap 'rm -rf "$appdir"' EXIT

install -Dm755 "$bin" "$appdir/usr/bin/dedup"
# Inside the AppImage the binary is found via PATH, not /usr/bin — rewrite Exec.
sed 's|^Exec=.*|Exec=dedup|' packaging/dedup.desktop \
  > "$appdir/dedup.desktop"
install -Dm644 "$appdir/dedup.desktop" "$appdir/usr/share/applications/dedup.desktop"
install -Dm644 crates/dedup-gui/assets/icon.png "$appdir/dedup.png"
install -Dm644 crates/dedup-gui/assets/icon.png \
  "$appdir/usr/share/icons/hicolor/256x256/apps/dedup.png"
ln -sf dedup.png "$appdir/.DirIcon"
# A wrapper rather than a symlink: exec'ing the real path keeps argv[0] =
# "dedup", so --help prints "Usage: dedup …" instead of "Usage: AppRun …".
install -Dm755 /dev/stdin "$appdir/AppRun" <<'EOF'
#!/bin/sh
exec "$(dirname "$0")/usr/bin/dedup" "$@"
EOF

# --- build the AppImage ------------------------------------------------------
mkdir -p "$OUTDIR"
out="$OUTDIR/dedup-${ver}-linux-${name_arch}.AppImage"
ARCH=$arch "$tool" --appimage-extract-and-run "$appdir" "$out" >/dev/null
echo "mkappimage: wrote $out"
