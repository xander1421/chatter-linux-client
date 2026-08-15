#!/bin/bash
# Assemble Chatter.app from an already-built chatter-ui binary.
#
#   macos/bundle.sh [debug|release] [signing-identity-hash]
#
# Copies every Homebrew dylib the binary pulls in (transitively) into
# Contents/Frameworks and rewrites the install names to @rpath.
#
# ponytail: GStreamer *plugins* (/opt/homebrew/lib/gstreamer-1.0, 275 files /
# 162M, each with its own Homebrew dep tree) are NOT bundled. libgstreamer has
# Homebrew's absolute plugin dir baked in, so media works on a machine that has
# brew gstreamer installed and nowhere else. Upgrade path when this ships to
# other machines: bundle the plugin subset actually used, and make the app
# re-exec itself through a shell stub that exports GST_PLUGIN_SYSTEM_PATH (an
# Info.plist LSEnvironment entry can't reference the bundle's own path).
set -euo pipefail

PROFILE="${1:-debug}"
IDENTITY="${2:-}"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$ROOT/target/$PROFILE/chatter-ui"
APP="$ROOT/target/$PROFILE/Chatter.app"

[ -x "$BIN" ] || { echo "no binary at $BIN — run: cargo build -p chatter-ui" >&2; exit 1; }

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Frameworks"

cp "$ROOT/macos/Info.plist" "$APP/Contents/Info.plist"
cp "$BIN" "$APP/Contents/MacOS/chatter-ui"
chmod +w "$APP/Contents/MacOS/chatter-ui"

# --- icon: build .icns from the PNGs already in assets/ ------------------
ICONSET="$(mktemp -d)/Chatter.iconset"
mkdir -p "$ICONSET"
for sz in 32 128 256; do
	[ -f "$ROOT/assets/icon-$sz.png" ] && cp "$ROOT/assets/icon-$sz.png" "$ICONSET/icon_${sz}x${sz}.png"
done
[ -f "$ROOT/assets/icon-64.png" ] && cp "$ROOT/assets/icon-64.png" "$ICONSET/icon_32x32@2x.png"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/Chatter.icns" 2>/dev/null \
	|| echo "warn: iconutil failed, shipping without icon" >&2

# --- Homebrew dylibs: transitive copy + install-name rewrite -------------
FW="$APP/Contents/Frameworks"
deps() { otool -L "$1" | tail -n +2 | awk '{print $1}' | grep -E '^(/opt/homebrew|/usr/local/(opt|Cellar))' || true; }

queue=$(deps "$APP/Contents/MacOS/chatter-ui")
while [ -n "$queue" ]; do
	next=""
	for lib in $queue; do
		base="$(basename "$lib")"
		[ -f "$FW/$base" ] && continue
		[ -f "$lib" ] || { echo "warn: missing $lib" >&2; continue; }
		cp "$lib" "$FW/$base"
		chmod +w "$FW/$base"
		next="$next $(deps "$FW/$base")"
	done
	queue="$next"
done

for f in "$FW"/*.dylib; do
	install_name_tool -id "@rpath/$(basename "$f")" "$f"
	for dep in $(deps "$f"); do
		install_name_tool -change "$dep" "@rpath/$(basename "$dep")" "$f"
	done
done

for dep in $(deps "$APP/Contents/MacOS/chatter-ui"); do
	install_name_tool -change "$dep" "@rpath/$(basename "$dep")" "$APP/Contents/MacOS/chatter-ui"
done
install_name_tool -add_rpath "@executable_path/../Frameworks" "$APP/Contents/MacOS/chatter-ui" 2>/dev/null || true

# --- sign ---------------------------------------------------------------
if [ -n "$IDENTITY" ]; then
	codesign --deep --force --options runtime \
		--entitlements "$ROOT/macos/chatter.entitlements" \
		--sign "$IDENTITY" "$APP"
else
	# Ad-hoc: the app still launches locally; TCC prompts just aren't stable.
	codesign --deep --force --sign - "$APP"
fi

echo "built $APP"
otool -L "$APP/Contents/MacOS/chatter-ui" | grep -c homebrew | xargs -I{} echo "remaining /opt/homebrew refs in main binary: {}"
