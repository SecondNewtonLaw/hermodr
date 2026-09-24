#!/usr/bin/env bash
#
# Builds and installs Hermóðr from the current checkout.
#
# Dependencies are declared in Cargo.toml: Tauri comes from crates.io, and
# whatsapp-rust is pinned to a git revision (the per-chunk history-sync control
# it needs is newer than the last release). Cargo fetches both, so there is
# nothing to clone by hand.
#
# Usage:
#   scripts/install-dev.sh          # build a release and install it
#   scripts/install-dev.sh --dev    # start the dev server
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

say() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

for tool in cargo node pnpm; do
  command -v "$tool" >/dev/null || die "$tool is required but not on PATH"
done

say "stopping any running instance"
pkill -9 -f 'target/debug/hermodr' 2>/dev/null || true
pkill -9 -f 'local/bin/hermodr' 2>/dev/null || true
pkill -9 -f 'WebKitWebProcess' 2>/dev/null || true
sleep 1

say "installing node dependencies"
(cd "$ROOT" && pnpm install)

if [ "${1:-}" = "--dev" ]; then
  say "starting the dev server"
  cd "$ROOT" && exec pnpm tauri dev
fi

say "building the frontend"
(cd "$ROOT" && pnpm build)

# Built with cargo rather than `pnpm tauri build`, so no bundle (and no
# AppImage tooling) is needed for a local install.
say "building the binary"
(cd "$ROOT/src-tauri" && cargo build --release)

say "installing the desktop entry"
BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/share/applications"
ICON_DIR="$HOME/.local/share/icons/hicolor/256x256/apps"
BIN="$ROOT/src-tauri/target/release/hermodr"
[ -x "$BIN" ] || die "release binary not found at $BIN"
mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR"
install -m 755 "$BIN" "$BIN_DIR/hermodr"
# `whatsapp` is the name people look for, so provide it as an alias.
ln -sf "$BIN_DIR/hermodr" "$BIN_DIR/whatsapp"
install -m 644 "$ROOT/src-tauri/icons/icon.png" "$ICON_DIR/hermodr.png"
cat > "$APP_DIR/hermodr.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Version=1.0
Name=Hermóðr
GenericName=WhatsApp Client
Comment=Native WhatsApp desktop client
Exec=$BIN_DIR/hermodr
Icon=hermodr
Terminal=false
Categories=Network;InstantMessaging;Chat;
Keywords=whatsapp;chat;messaging;hermodr;
StartupWMClass=hermodr
DESKTOP
if command -v update-desktop-database >/dev/null; then
  update-desktop-database "$APP_DIR" || true
fi

say "done: hermodr installed (also as \"whatsapp\")"
say "make sure $BIN_DIR is on your PATH"

# System packages are not installed here on purpose. What the build needs beyond
# the three tools above:
#
#   Arch       webkit2gtk-4.1 gtk3 libappindicator-gtk3 librsvg pkgconf openssl
#   Debian     libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
#              librsvg2-dev libssl-dev pkg-config build-essential
