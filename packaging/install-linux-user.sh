#!/bin/bash
# Install the already-built Linux desktop client for the current user.
#
#   packaging/install-linux-user.sh
#
# Copies target/release/chatter-ui to ~/.local/bin, installs
# assets/chatter-linux.desktop as the desktop-file id `chatter-linux`, and
# installs the hicolor icons under that same name. Exec is rewritten to the
# absolute binary path so a desktop session does not depend on PATH.
#
# This script does not build, does not start the app, does not read the
# database, and does not set CHATTER_NO_KEYRING or CHATTER_DB_PASSPHRASE.
# A normal launch uses the OS keyring (Secret Service).
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
PROFILE="${PROFILE:-release}"
BIN="$ROOT/target/$PROFILE/chatter-ui"
DATA_HOME="${XDG_DATA_HOME:-$HOME/.local/share}"
BIN_DIR="$HOME/.local/bin"
APP_DIR="$DATA_HOME/applications"
ICON_ROOT="$DATA_HOME/icons/hicolor"

if [ ! -x "$BIN" ]; then
    echo "no binary at $BIN" >&2
    echo "build: ../scripts/anchors.sh exec cargo build --release" >&2
    exit 1
fi

mkdir -p "$BIN_DIR" "$APP_DIR"
install -m 0755 "$BIN" "$BIN_DIR/chatter-ui"

desktop="$APP_DIR/chatter-linux.desktop"
sed "s|^Exec=chatter-ui$|Exec=$BIN_DIR/chatter-ui|" \
    "$ROOT/assets/chatter-linux.desktop" > "$desktop"
chmod 0644 "$desktop"

while read -r size src; do
    [ -f "$ROOT/assets/$src" ] || continue
    dest="$ICON_ROOT/${size}x${size}/apps"
    mkdir -p "$dest"
    install -m 0644 "$ROOT/assets/$src" "$dest/chatter-linux.png"
done <<'ICONS'
16 icon-16.png
24 icon-24.png
32 icon-32.png
48 icon-48.png
64 icon-64.png
128 icon-128.png
256 icon-256.png
512 icon-512.png
ICONS

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APP_DIR" || true
fi

echo "installed $BIN_DIR/chatter-ui"
echo "desktop  $desktop"
echo "launch uses the OS keyring; this script did not read or write it"
