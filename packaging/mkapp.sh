#!/usr/bin/env bash
# Assemble the macOS app bundle (dedup.app) and a .dmg around it, from an
# already-built `dedup` binary. macOS-only: uses sips/iconutil for the .icns
# and hdiutil for the .dmg. The bundle is unsigned — first launch needs
# right-click → Open (documented in the README); signing/notarization is a
# later wave.
#
# Usage:
#   packaging/mkapp.sh <path-to-dedup-binary> <version> <out-dir>
#
# Writes <out-dir>/dedup.app and <out-dir>/dedup-<version>-macos-<arch>.dmg.
set -euo pipefail

[ $# -eq 3 ] || { sed -n '2,11p' "$0"; exit 2; }
BIN=$1
VERSION=$2
OUTDIR=$3

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
ICON_PNG="$ROOT/crates/dedup-gui/assets/icon.png"
[ -f "$BIN" ] || { echo "mkapp: binary not found: $BIN" >&2; exit 1; }
[ -f "$ICON_PNG" ] || { echo "mkapp: icon not found: $ICON_PNG" >&2; exit 1; }

ARCH=$(uname -m)          # arm64 on Apple silicon, x86_64 on Intel
APP="$OUTDIR/dedup.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# --- binary ------------------------------------------------------------------
install -m755 "$BIN" "$APP/Contents/MacOS/dedup"

# --- icon: iconset from the 256px PNG, upscaled/downscaled by sips ------------
ICONSET=$(mktemp -d)/dedup.iconset
mkdir -p "$ICONSET"
for size in 16 32 64 128 256 512; do
  sips -z $size $size "$ICON_PNG" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z $double $double "$ICON_PNG" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/dedup.icns"

# --- Info.plist ----------------------------------------------------------------
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>dedup</string>
  <key>CFBundleDisplayName</key><string>dedup</string>
  <key>CFBundleIdentifier</key><string>dev.paxel.dedup</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>dedup</string>
  <key>CFBundleIconFile</key><string>dedup</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# --- dmg ----------------------------------------------------------------------
DMG="$OUTDIR/dedup-${VERSION}-macos-${ARCH}.dmg"
rm -f "$DMG"
hdiutil create -volname "dedup ${VERSION}" -srcfolder "$APP" -ov -format UDZO "$DMG" >/dev/null
echo "mkapp: wrote $APP and $DMG"
