#!/bin/bash
# Assemble Chatter.app from an already-built chatter-ui binary.
#
#   macos/bundle.sh [debug|release] [signing-identity-hash]
#
# Copies every Homebrew dylib the binary pulls in (transitively) into
# Contents/Frameworks and rewrites the install names to @rpath. Also copies
# every GStreamer plugin (/opt/homebrew/lib/gstreamer-1.0) into
# Contents/Resources/gstreamer-1.0-plugins and rewrites THEIR dependency references to
# @rpath the same way — see the "why" comment above the plugin loop below.
#
# GST_PLUGIN_SYSTEM_PATH must point at Contents/Resources/gstreamer-1.0-plugins.
# Info.plist LSEnvironment cannot express a bundle-relative path. The desktop
# binary sets it from its own executable path when this directory exists
# (platform/macos.rs) and leaves a caller-supplied value alone. `open` and
# double-click therefore do not need a wrapper.
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
# iconset name -> source png. The @2x entries are what Retina actually draws;
# without them macOS upscales the 1x and the icon looks soft in the Dock.
while read -r name src; do
	[ -f "$ROOT/assets/$src" ] && cp "$ROOT/assets/$src" "$ICONSET/$name"
done <<-'ICONS'
	icon_16x16.png        icon-16.png
	icon_16x16@2x.png     icon-32.png
	icon_32x32.png        icon-32.png
	icon_32x32@2x.png     icon-64.png
	icon_128x128.png      icon-128.png
	icon_128x128@2x.png   icon-256.png
	icon_256x256.png      icon-256.png
	icon_256x256@2x.png   icon-512.png
	icon_512x512.png      icon-512.png
	icon_512x512@2x.png   icon-1024.png
ICONS
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

# --- GStreamer plugins: same treatment, or two copies of GLib collide ----
#
# Plugins are dlopen'd at runtime (GST_PLUGIN_SYSTEM_PATH), not linked at
# build time, so the transitive walk above never sees them. Left pointing
# at their original Homebrew absolute paths, each plugin loads ITS OWN
# libglib/libgobject instead of resolving to the @rpath copy already
# loaded from Contents/Frameworks — two live copies of the same library in
# one process corrupts GObject's type table (G_IS_PARAM_SPEC assertions,
# "type id '0' is invalid") and segfaults on whichever plugin the scanner
# happens to touch next (seen with both libgstwebp and libgstinterleave —
# not plugin-specific, any of them can trip it). Rewriting every plugin's
# deps to @rpath, same as the app's own Frameworks copies, makes them
# resolve to the SAME already-loaded instance instead of a second one.
PLUGDIR="$APP/Contents/Resources/gstreamer-1.0-plugins"
mkdir -p "$PLUGDIR"
SRC_PLUGDIR="/opt/homebrew/lib/gstreamer-1.0"
if [ -d "$SRC_PLUGDIR" ]; then
	cp "$SRC_PLUGDIR"/*.dylib "$PLUGDIR/" 2>/dev/null || true

	# Any dep a plugin needs that isn't already in Frameworks (i.e. not
	# reachable from the app binary's own transitive walk) gets pulled in
	# here too, same copy+rewrite pattern as the main loop above.
	queue=""
	for f in "$PLUGDIR"/*.dylib; do
		queue="$queue $(deps "$f")"
	done
	while [ -n "$queue" ]; do
		next=""
		for lib in $queue; do
			base="$(basename "$lib")"
			[ -f "$FW/$base" ] && continue
			[ -f "$lib" ] || continue
			cp "$lib" "$FW/$base"
			chmod +w "$FW/$base"
			install_name_tool -id "@rpath/$base" "$FW/$base"
			for dep in $(deps "$FW/$base"); do
				install_name_tool -change "$dep" "@rpath/$(basename "$dep")" "$FW/$base"
			done
			next="$next $(deps "$FW/$base")"
		done
		queue="$next"
	done

	for f in "$PLUGDIR"/*.dylib; do
		chmod +w "$f"
		for dep in $(deps "$f"); do
			install_name_tool -change "$dep" "@rpath/$(basename "$dep")" "$f" 2>/dev/null || true
		done
		install_name_tool -add_rpath "@executable_path/../Frameworks" "$f" 2>/dev/null || true
	done
else
	echo "warn: no GStreamer plugin dir at $SRC_PLUGDIR — camera/media will be limited to whatever's statically linked" >&2
fi

# --- sign -----------------------------------------------------------------
#
# `codesign --deep` walks known embedded-code locations (Contents/Frameworks,
# Contents/PlugIns, nested .app bundles) but NOT loose files under
# Contents/Resources — which is exactly where the GStreamer plugins above
# live (PlugIns is reserved for nested bundles, see the comment at PLUGDIR).
# install_name_tool already invalidated every plugin's original signature by
# rewriting its load commands; left unsigned, dlopen'ing any of them at
# runtime gets SIGKILL'd by the kernel (EXC_BAD_ACCESS / Code Signature
# Invalid) the moment GStreamer's plugin scanner touches one. Sign every
# loose dylib individually before the deep bundle-level pass covers the rest.
SIGN_ARGS=(--force --sign -)
[ -n "$IDENTITY" ] && SIGN_ARGS=(--force --options runtime --sign "$IDENTITY")
for f in "$FW"/*.dylib "$PLUGDIR"/*.dylib; do
	[ -f "$f" ] || continue
	codesign "${SIGN_ARGS[@]}" "$f"
done

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
