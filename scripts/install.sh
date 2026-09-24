#!/usr/bin/env bash
#
# Installs Hermóðr from the latest GitHub release. It downloads the AppImage,
# puts it on PATH, and adds a desktop entry so it shows up as WhatsApp.
#
#   curl -fsSL https://raw.githubusercontent.com/emiliano-go/hermodr/master/scripts/install.sh | sh
#
set -euo pipefail

REPO="emiliano-go/hermodr"
ASSET="Hermodr-x86_64.AppImage"
BASE="https://github.com/$REPO/releases/latest/download"

say() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

command -v curl >/dev/null || die "curl is required"

BIN_DIR="$HOME/.local/bin"
APP_DIR="$HOME/.local/share/applications"
ICON_DIR="$HOME/.local/share/icons/hicolor/256x256/apps"
TARGET="$BIN_DIR/hermodr"

mkdir -p "$BIN_DIR" "$APP_DIR" "$ICON_DIR"

say "downloading the latest release"
curl -fL --proto '=https' --tlsv1.2 -o "$TARGET" "$BASE/$ASSET"
chmod +x "$TARGET"

# `whatsapp` is the name people look for, so provide it as an alias.
ln -sf "$TARGET" "$BIN_DIR/whatsapp"

say "installing the icon and desktop entry"
curl -fL --proto '=https' --tlsv1.2 -o "$ICON_DIR/hermodr.png" \
  "https://raw.githubusercontent.com/$REPO/master/src-tauri/icons/icon.png"

cat > "$APP_DIR/hermodr.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Version=1.0
Name=Hermóðr
GenericName=WhatsApp Client
Comment=Native WhatsApp desktop client
Exec=$TARGET
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
