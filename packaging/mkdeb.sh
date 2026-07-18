#!/usr/bin/env bash
# Build a Debian package (.deb) for the `dedup` binary.
#
# Produces a snapshot by default (version suffixed with a sortable
# ~snapshot<timestamp>.<git-sha>, so it always sorts *older* than the matching
# release) or a clean release build with --release.
#
# The runtime Depends are derived from the built binary's actual DT_NEEDED
# libraries (via `objdump` + `dpkg -S`) so they are correct for whatever Debian
# release you build on — the ALSA package, for instance, is `libasound2` on
# bookworm but `libasound2t64` on trixie. The GUI's OpenGL / X11 / Wayland /
# desktop-portal libraries are dlopen'd at runtime (not linked), so they cannot
# be auto-detected and are listed as Recommends instead — the CLI works without
# them, and a desktop pulls them in automatically.
#
# Usage:
#   packaging/mkdeb.sh [--snapshot|--release] [--target <triple>]
#                      [--version <ver>] [--out <dir>] [--no-build]
#
# Examples:
#   packaging/mkdeb.sh                       # snapshot .deb for the host arch
#   packaging/mkdeb.sh --release             # clean release .deb, host arch
#   packaging/mkdeb.sh --release --target aarch64-unknown-linux-gnu
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
    -h|--help)  sed -n '2,26p' "$0"; exit 0 ;;
    *) echo "mkdeb: unknown argument: $1" >&2; exit 2 ;;
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
build_flags=(--release -p dedup-cli --locked)
[ -n "$TARGET" ] && build_flags+=(--target "$TARGET")
if [ "$DO_BUILD" -eq 1 ]; then
  echo "mkdeb: building dedup ($MODE, version $ver${TARGET:+, target $TARGET})…"
  cargo build "${build_flags[@]}"
fi
bin="target/${TARGET:+$TARGET/}release/dedup"
[ -x "$bin" ] || { echo "mkdeb: binary not found at $bin (drop --no-build?)" >&2; exit 1; }

# --- architecture ------------------------------------------------------------
if [ -n "$TARGET" ]; then
  case "$TARGET" in
    x86_64-*)          arch=amd64 ;;
    aarch64-*)         arch=arm64 ;;
    armv7-*|arm-*)     arch=armhf ;;
    i686-*|i586-*)     arch=i386 ;;
    *) echo "mkdeb: cannot map target '$TARGET' to a Debian arch" >&2; exit 1 ;;
  esac
elif command -v dpkg >/dev/null 2>&1; then
  arch=$(dpkg --print-architecture)
else
  echo "mkdeb: no --target and no dpkg to detect the host arch" >&2; exit 1
fi

# --- derive Depends from the binary's DT_NEEDED ------------------------------
# Falls back to the core set when dpkg/objdump are unavailable (e.g. a
# non-Debian cross-build host); auto-detection is preferred because it names the
# packages exactly as they exist on the build distribution.
derive_depends() {
  local b=$1 so pkg
  local -a pkgs=()
  if ! command -v objdump >/dev/null 2>&1 || ! command -v dpkg-query >/dev/null 2>&1; then
    echo "libc6, libgcc-s1, libasound2"
    return
  fi
  while read -r so; do
    [ -n "$so" ] || continue
    pkg=$(dpkg-query -S "*/$so" 2>/dev/null | head -1 | cut -d: -f1) || true
    [ -n "${pkg:-}" ] && pkgs+=("$pkg")
  done < <(objdump -p "$b" 2>/dev/null | awk '/NEEDED/{print $2}')
  if [ ${#pkgs[@]} -eq 0 ]; then
    echo "libc6, libgcc-s1, libasound2"
    return
  fi
  # Join the unique package names with a literal ", " (paste -d cycles through
  # single-char delimiters, so it can't produce a two-char separator).
  printf '%s\n' "${pkgs[@]}" | sort -u \
    | awk 'NR>1{printf ", "} {printf "%s", $0} END{if (NR) print ""}'
}
depends=$(derive_depends "$bin")

# The GUI loads these at runtime via dlopen / the desktop portal, so shlibdeps
# cannot see them; Recommends keeps a headless install slim while a desktop
# still gets a working GUI. ffmpeg powers the optional video fingerprint.
recommends="libgl1, libx11-6, libxkbcommon0, libwayland-client0, xdg-desktop-portal, ffmpeg"

# --- stage the package tree --------------------------------------------------
pkgdir=$(mktemp -d)
trap 'rm -rf "$pkgdir"' EXIT
chmod 755 "$pkgdir"   # mktemp gives 700; the package root must be world-readable

install -Dm755 "$bin"                              "$pkgdir/usr/bin/dedup"
install -Dm644 packaging/dedup.desktop             "$pkgdir/usr/share/applications/dedup.desktop"
install -Dm644 crates/dedup-gui/assets/icon.png    "$pkgdir/usr/share/icons/hicolor/256x256/apps/dedup.png"
install -Dm644 README.md                           "$pkgdir/usr/share/doc/dedup/README.md"

# copyright (LICENSE is the project's; point at it)
license_line=$(head -1 LICENSE 2>/dev/null || echo "See the project LICENSE file.")
install -Dm644 /dev/stdin "$pkgdir/usr/share/doc/dedup/copyright" <<EOF
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: dedup-rs
Source: https://sr.ht/~yourusername/dedup-rs

Files: *
Copyright: Patrick Zimmer <taum@tuta.io>
License: $license_line
EOF

# minimal Debian changelog (dpkg/lintian expect one, gzipped -9n)
install -Dm644 /dev/stdin "$pkgdir/usr/share/doc/dedup/changelog.Debian" <<EOF
dedup ($ver) unstable; urgency=medium

  * Packaged from the upstream tree by packaging/mkdeb.sh.

 -- Patrick Zimmer <taum@tuta.io>  $(date -uR)
EOF
gzip -9n "$pkgdir/usr/share/doc/dedup/changelog.Debian"

# --- control -----------------------------------------------------------------
installed_kb=$(du -k -s "$pkgdir/usr" | cut -f1)
mkdir -p "$pkgdir/DEBIAN"
cat > "$pkgdir/DEBIAN/control" <<EOF
Package: dedup
Version: $ver
Section: utils
Priority: optional
Architecture: $arch
Maintainer: Patrick Zimmer <taum@tuta.io>
Installed-Size: $installed_kb
Depends: $depends
Recommends: $recommends
Description: forensic data-inheritance triage tool
 dedup finds the useful and important material — documents, photos, keys,
 crypto wallets — buried in heaps of redundant backups left on old disks and
 NAS drives, without eyeballing terabytes of duplicates.
 .
 A single binary provides both the command-line interface and the desktop GUI
 (Repositories / Duplicates / Transfer / Grooming). Content identity is size +
 BLAKE3, with perceptual fingerprints for images, video, audio and documents.
EOF

# --- build the .deb ----------------------------------------------------------
mkdir -p "$OUTDIR"
out="$OUTDIR/dedup_${ver}_${arch}.deb"
dpkg-deb --root-owner-group --build "$pkgdir" "$out" >/dev/null
echo "mkdeb: wrote $out"

# Optional sanity check (never fatal — lintian is advisory here).
if command -v lintian >/dev/null 2>&1; then
  lintian --no-tag-display-limit "$out" || true
fi
