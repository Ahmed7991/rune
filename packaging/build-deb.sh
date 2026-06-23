#!/usr/bin/env bash
# Build a Debian/Ubuntu .deb for rune. Produces dist/rune_<version>_amd64.deb.
# A system install (/usr/bin) that apt resolves the GTK4/VTE/rsvg deps for —
# the one-step install for Debian/Ubuntu. For other distros, build from source
# (see packaging/install.sh / the README).
set -euo pipefail

cd "$(dirname "$0")/.."
VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
ARCH=$(dpkg --print-architecture)
PKG="rune_${VERSION}_${ARCH}"
ROOT="target/deb/${PKG}"

echo "→ building release binary"
RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME/.cargo=/cargo --remap-path-prefix=$HOME=/build" cargo build --release

echo "→ assembling ${PKG}.deb"
rm -rf "$ROOT"
install -Dm0755 target/release/rune                         "$ROOT/usr/bin/rune"
install -Dm0644 packaging/io.github.ahmed7991.rune.svg               "$ROOT/usr/share/icons/hicolor/scalable/apps/io.github.ahmed7991.rune.svg"

# A system .desktop: Exec on PATH (the user install.sh pins ~/.local/bin instead).
install -d "$ROOT/usr/share/applications"
sed 's#^Exec=.*#Exec=rune#; s#^Icon=.*#Icon=io.github.ahmed7991.rune#' \
    packaging/io.github.ahmed7991.rune.desktop > "$ROOT/usr/share/applications/io.github.ahmed7991.rune.desktop"

SIZE_KB=$(du -ks "$ROOT/usr" | cut -f1)
install -d "$ROOT/DEBIAN"
cat > "$ROOT/DEBIAN/control" <<EOF
Package: rune
Version: ${VERSION}
Architecture: ${ARCH}
Maintainer: Ahmed7991 <Ahmed7991@users.noreply.github.com>
Installed-Size: ${SIZE_KB}
Depends: libgtk-4-1 (>= 4.10), libvte-2.91-gtk4-0, librsvg2-common
Section: utils
Priority: optional
Homepage: https://github.com/Ahmed7991/rune
Description: Native control cockpit for Claude Code sessions
 rune is a native (GTK4 + VTE, no webview) Linux cockpit for managing
 Claude Code sessions across projects: a tabbed terminal host, a
 cross-project "needs-you" queue, live status, per-project launch presets,
 and reply-from-the-dashboard. Reads files written by the official claude
 CLI and spawns it as a subprocess. Not affiliated with Anthropic.
EOF

mkdir -p dist
fakeroot dpkg-deb --build --root-owner-group "$ROOT" "dist/${PKG}.deb" >/dev/null
echo "→ built dist/${PKG}.deb"
dpkg-deb -I "dist/${PKG}.deb" | sed -n '/Package:/,/Description:/p'
echo "--- contents ---"; dpkg-deb -c "dist/${PKG}.deb" | awk '{print $1, $6}'
