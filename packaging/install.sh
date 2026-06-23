#!/usr/bin/env bash
# Install (or uninstall) rune as a real desktop app for the current user.
# No root needed — everything lands under ~/.local. Reversible: `install.sh uninstall`.
set -euo pipefail

repo="$(cd "$(dirname "$0")/.." && pwd)"
app_id="io.github.ahmed7991.rune"
bin_dir="$HOME/.local/bin"
desktop_dir="$HOME/.local/share/applications"
icon_dir="$HOME/.local/share/icons/hicolor/scalable/apps"

uninstall() {
  rm -f "$bin_dir/rune" \
        "$desktop_dir/$app_id.desktop" \
        "$icon_dir/$app_id.svg"
  command -v update-desktop-database >/dev/null && update-desktop-database "$desktop_dir" 2>/dev/null || true
  command -v gtk-update-icon-cache  >/dev/null && gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true
  echo "rune uninstalled."
}

if [ "${1:-install}" = "uninstall" ]; then uninstall; exit 0; fi

# Build the optimized binary.
( cd "$repo" && cargo build --release )

mkdir -p "$bin_dir" "$desktop_dir" "$icon_dir"
install -m755 "$repo/target/release/rune" "$bin_dir/rune"
install -m644 "$repo/packaging/$app_id.svg" "$icon_dir/$app_id.svg"
# Pin Exec to the absolute installed path so it launches regardless of the
# desktop session's PATH.
sed "s|^Exec=rune\$|Exec=$bin_dir/rune|" "$repo/packaging/$app_id.desktop" \
  > "$desktop_dir/$app_id.desktop"
chmod 644 "$desktop_dir/$app_id.desktop"

command -v update-desktop-database >/dev/null && update-desktop-database "$desktop_dir" 2>/dev/null || true
command -v gtk-update-icon-cache  >/dev/null && gtk-update-icon-cache -f -t "$HOME/.local/share/icons/hicolor" 2>/dev/null || true

echo "rune installed:"
echo "  binary  : $bin_dir/rune"
echo "  desktop : $desktop_dir/$app_id.desktop"
echo "  icon    : $icon_dir/$app_id.svg"
echo "Launch it from your app grid (search 'rune'), or run: rune"
