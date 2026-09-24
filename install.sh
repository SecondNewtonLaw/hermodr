#!/usr/bin/env bash
#
# Builds Hermóðr from a clean checkout.
#
# Everything is pinned: what Hermóðr depends on is not the crates.io releases
# but two local checkouts. `whatsapp-rust` carries the per-chunk history-sync
# control that only exists at one revision, and `tauri` carries a patch that
# mounts child webviews in a GtkFixed so the UI and the composer coexist.
#
# The two checkouts are expected as siblings of this repository, because the
# Cargo manifests reference them by relative path:
#
#   <parent>/whatsapp-rust      <-  ../whatsapp-rust
#   <parent>/tauri              <-  ../../tauri   (from src-tauri)
#
# Usage:
#   ./install.sh          # clone the dependencies and build a release
#   ./install.sh --dev    # clone the dependencies and start the dev server
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PARENT="$(dirname "$ROOT")"

WHATSAPP_RUST_REPO="${WHATSAPP_RUST_REPO:-https://github.com/oxidezap/whatsapp-rust}"
WHATSAPP_RUST_REV="9eb43b9bc2bc561cc34a672b4295528bf4c2d733"
TAURI_REPO="${TAURI_REPO:-https://github.com/tauri-apps/tauri}"
TAURI_REV="7b95549990f34048e77b41e5a2b8961c3e469e94"

WHAT_WHATSAPP_RUST="$PARENT/whatsapp-rust"
WHAT_TAURI="$PARENT/tauri"
PATCH="$ROOT/patches/tauri-gtk-fixed.patch"

say() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
die() { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

for tool in git cargo node pnpm; do
  command -v "$tool" >/dev/null || die "$tool is required but not on PATH"
done

# Checks out a repository at an exact revision, cloning it if needed.
pin() {
  local repo="$1" dir="$2" rev="$3"
  if [ -d "$dir/.git" ]; then
    say "$(basename "$dir") already present, pinning to ${rev:0:9}"
    git -C "$dir" fetch --quiet origin "$rev" 2>/dev/null || true
  else
    say "cloning $(basename "$dir")"
    git clone --quiet "$repo" "$dir"
    git -C "$dir" fetch --quiet origin "$rev" 2>/dev/null || true
  fi
  git -C "$dir" checkout --quiet "$rev" \
    || die "could not check out $rev in $dir (is the revision still reachable upstream?)"
}

pin "$WHATSAPP_RUST_REPO" "$WHAT_WHATSAPP_RUST" "$WHATSAPP_RUST_REV"
pin "$TAURI_REPO" "$WHAT_TAURI" "$TAURI_REV"

# The Tauri patch is applied on top of the pinned revision. Re-applying is
# harmless (git apply checks first), and a fresh clone needs it.
if git -C "$WHAT_TAURI" apply --check "$PATCH" 2>/dev/null; then
  say "applying the GtkFixed patch to tauri"
  git -C "$WHAT_TAURI" apply "$PATCH"
elif git -C "$WHAT_TAURI" apply --reverse --check "$PATCH" 2>/dev/null; then
  say "tauri patch already applied"
else
  die "the tauri patch does not apply; $WHAT_TAURI is not the expected revision"
fi

say "installing node dependencies"
(cd "$ROOT" && pnpm install)

if [ "${1:-}" = "--dev" ]; then
  say "starting the dev server"
  exec pnpm tauri dev
fi

say "building a release bundle"
pnpm tauri build

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

# System packages are not installed here on purpose: a script that quietly runs
# a package manager is a script that quietly runs a package manager. What the
# build needs beyond the three tools above:
#
#   Arch       webkit2gtk-4.1 gtk3 libappindicator-gtk3 librsvg pkgconf openssl
#   Debian     libwebkit2gtk-4.1-dev libgtk-3-dev libayatana-appindicator3-dev \
#              librsvg2-dev libssl-dev pkg-config build-essential
