#!/usr/bin/env sh
# Install the dedup app icon + .desktop entry into the current user's XDG dirs
# so Wayland/KDE (and X11 desktops) show the window/taskbar icon. Idempotent.
#
# Wayland ignores the programmatic window icon; the compositor instead matches
# the window's app_id ("dedup") to <app_id>.desktop and uses its Icon= entry.
set -eu

repo_dir=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
icon_src="$repo_dir/crates/dedup-gui/assets/icon.png"
desktop_src="$repo_dir/packaging/dedup.desktop"

data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
icon_dst="$data_home/icons/hicolor/256x256/apps/dedup.png"
desktop_dst="$data_home/applications/dedup.desktop"

mkdir -p "$(dirname "$icon_dst")" "$(dirname "$desktop_dst")"
cp "$icon_src" "$icon_dst"
cp "$desktop_src" "$desktop_dst"

# Refresh caches where the tools exist (harmless if they don't).
command -v gtk-update-icon-cache >/dev/null 2>&1 &&
  gtk-update-icon-cache -f -t "$data_home/icons/hicolor" >/dev/null 2>&1 || true
command -v update-desktop-database >/dev/null 2>&1 &&
  update-desktop-database "$data_home/applications" >/dev/null 2>&1 || true
command -v kbuildsycoca6 >/dev/null 2>&1 && kbuildsycoca6 >/dev/null 2>&1 || true

echo "Installed:"
echo "  $icon_dst"
echo "  $desktop_dst"
echo "Restart the app; on KDE you may need to log out/in (or restart plasmashell) for the taskbar to pick it up."
