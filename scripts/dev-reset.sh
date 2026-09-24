#!/usr/bin/env bash
#
# Restarts the dev server so visual changes show up without rebuilding and
# reinstalling. It stops whatever is running first, so it is safe to run while
# an instance is up.
#
# Usage:
#   pnpm reset        # or: bash scripts/dev-reset.sh
#
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

say() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }

say "stopping the running instance"
pkill -9 -f 'target/debug/hermodr' 2>/dev/null || true
pkill -9 -f 'local/bin/hermodr' 2>/dev/null || true
pkill -9 -f 'tauri.js dev' 2>/dev/null || true
pkill -9 -f 'vite.js dev' 2>/dev/null || true
pkill -9 -f 'WebKitWebProcess' 2>/dev/null || true
sleep 1

say "starting the dev server"
cd "$ROOT"
exec pnpm tauri dev
