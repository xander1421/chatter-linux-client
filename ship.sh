#!/bin/bash
# Build, sign, install and launch Chatter on macOS and/or a connected iPhone.
#
#   ./ship.sh doctor      what's ready, what a human still has to click
#   ./ship.sh mac         build + bundle + sign + launch Chatter.app
#   ./ship.sh ios         build + sign + install + launch on the phone
#   ./ship.sh all         both
#   ./ship.sh shot        screenshot the app on the phone
#   ./ship.sh tap X Y     tap normalized coords (0..1), then screenshot
#   ./ship.sh drive 'A|B' several steps in one app session ("x,y" or
#                         "wait:SECS"), screenshotting after each.
#                         No typing — see the note in ios/UITests/DeviceControl.swift
#
# Env:
#   PROFILE=debug|release   cargo profile (default debug)
#   DEVICE=<udid>           override the target iPhone
#   CHATTER_IOS_FEATURES=ui build the Slint UI instead of the smoke harness
#   CHATTER_SERVER_URL=...  gRPC server baked into Info.plist (default: the LAN
#                           address in ios/project.yml). main.m re-exports it as
#                           an env var before Rust boots.
#
# no Makefile, no just, no cargo-xtask — this is four commands in a
# trenchcoat. If it grows a dependency graph, that's when it earns a real task
# runner.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROFILE="${PROFILE:-debug}"
DEVICE="${DEVICE:-A5F11F2B-13C6-5832-9B01-61EA90D53A0B}"
BUNDLE_ID="com.chatter.client"

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
fail() { printf '\033[31mBLOCKED: %s\033[0m\n' "$*" >&2; exit 1; }

# The signing identity's team is the cert's OU, NOT the suffix in the CN.
# (CN says 2FW5QDSBCQ, which is the cert id; the team is OU=75A5A8N59P.)
signing_identity() {
  security find-identity -v -p codesigning 2>/dev/null |
    awk '/Apple Develop/ {print $2; exit}'
}

# ---------------------------------------------------------------- doctor

doctor() {
  local blocked=0

  say "toolchain"
  for t in cargo xcodebuild xcodegen xcrun; do
    command -v "$t" >/dev/null && echo "  ok   $t" || { echo "  MISS $t"; blocked=1; }
  done
  rustup target list --installed 2>/dev/null | grep -q aarch64-apple-ios \
    && echo "  ok   rust ios target" || { echo "  MISS rustup target add aarch64-apple-ios"; blocked=1; }

  say "disk"
  # Rust link steps for this tree need headroom; target-ios alone is ~8G.
  local free_g; free_g=$(df -g "$ROOT" | awk 'NR==2 {print $4}')
  if [ "$free_g" -lt 20 ]; then
    echo "  LOW  ${free_g}G free — builds will fail. Reclaim with:"
    echo "         rm -rf $ROOT/target-ios   # ~8G, rebuilds in ~15min"
    blocked=1
  else
    echo "  ok   ${free_g}G free"
  fi

  say "macOS signing"
  local id; id="$(signing_identity)"
  [ -n "$id" ] && echo "  ok   identity $id" || { echo "  MISS no codesigning identity"; blocked=1; }

  say "iOS signing"
  # Check for an actual profile, NOT DVTDeveloperAccountManagerAppleIDLists —
  # that key stayed empty even after a successful sign-in and provisioning, so
  # it produces false negatives. A profile on disk is the honest signal.
  local prof found=""
  for prof in "$HOME/Library/Developer/Xcode/UserData/Provisioning Profiles"/*.mobileprovision; do
    [ -f "$prof" ] || continue
    security cms -D -i "$prof" 2>/dev/null | grep -q "$BUNDLE_ID" && { found="$prof"; break; }
  done
  if [ -n "$found" ]; then
    echo "  ok   profile for $BUNDLE_ID ($(basename "$found"))"
    echo "       free Personal Team profiles expire after 7 days — re-run then"
  else
    echo "  MISS no provisioning profile for $BUNDLE_ID."
    echo "         Open ios/Chatter.xcodeproj, target Chatter > Signing & Capabilities,"
    echo "         tick Automatically manage signing, pick the Personal Team."
    echo "         Needs your Apple ID password and 2FA. Cannot be scripted."
    blocked=1
  fi

  say "device"
  if xcrun devicectl list devices 2>/dev/null | grep -q "$DEVICE.*connected"; then
    echo "  ok   $DEVICE connected"
    echo "       (launch is refused while the phone is locked — unlock it)"
  else
    echo "  MISS device $DEVICE not connected"; blocked=1
  fi

  return $blocked
}

# ---------------------------------------------------------------- macOS

mac() {
  local id; id="$(signing_identity)"
  [ -n "$id" ] || fail "no codesigning identity; see ./ship.sh doctor"

  say "cargo build -p chatter-ui ($PROFILE)"
  ( cd "$ROOT" && cargo build -p chatter-ui $( [ "$PROFILE" = release ] && echo --release ) )

  say "bundle + sign"
  "$ROOT/macos/bundle.sh" "$PROFILE" "$id"

  local app="$ROOT/target/$PROFILE/Chatter.app"
  codesign --verify --deep --strict "$app"
  echo "signed ok: $app"

  # CHATTER_DB_PASSPHRASE bypasses the keyring (keyring.rs:81). Without it the
  # Developer-signed bundle is a different code identity from the binary that
  # created the keychain item, so macOS raises a blocking modal and startup
  # hangs until a human clicks Always Allow.
  #
  # `open --env`, not `VAR=x open`: launchd starts the app, so a prefixed
  # assignment reaches `open` and dies there, never the app.
  say "launch"
  open --env "CHATTER_DB_PASSPHRASE=${CHATTER_DB_PASSPHRASE:-dev}" "$app"
  echo "launched. spctl will reject this bundle (Development cert, not Developer ID) — expected."
}

# ---------------------------------------------------------------- iOS

ios() {
  say "xcodegen"
  ( cd "$ROOT/ios" && xcodegen generate )

  say "xcodebuild ($PROFILE)"
  local derived="$ROOT/target-ios/DerivedData"
  # CHATTER_IOS_FEATURES=ui swaps the ObjC smoke shell for the Rust/Slint entry
  # point. The two must move together: main.m only calls chatter_ios_run_ui()
  # when CHATTER_RUST_UI is defined, and that symbol only exists under the
  # cargo `ui` feature. Setting one without the other fails at link.
  local defs=()
  case "${CHATTER_IOS_FEATURES:-}" in
    *ui*) defs=(GCC_PREPROCESSOR_DEFINITIONS='$(inherited) CHATTER_RUST_UI=1')
          echo "  (Slint UI build)" ;;
  esac
  xcodebuild -project "$ROOT/ios/Chatter.xcodeproj" -scheme Chatter \
    -configuration "$( [ "$PROFILE" = release ] && echo Release || echo Debug )" \
    -destination "platform=iOS,id=$DEVICE" \
    -derivedDataPath "$derived" \
    ${defs+"${defs[@]}"} \
    ${CHATTER_SERVER_URL:+CHATTER_SERVER_URL="$CHATTER_SERVER_URL"} \
    -allowProvisioningUpdates build \
    || fail "build failed. If it says 'No Accounts' or 'No profiles for $BUNDLE_ID', run ./ship.sh doctor — you need an Apple ID in Xcode."

  local app
  app=$(find "$derived/Build/Products" -maxdepth 2 -name 'Chatter.app' -print -quit)
  [ -n "$app" ] || fail "built, but no Chatter.app under $derived"

  say "install"
  xcrun devicectl device install app --device "$DEVICE" "$app"

  say "launch"
  # Locked phones refuse launch even after a successful install.
  xcrun devicectl device process launch --device "$DEVICE" "$BUNDLE_ID" \
    || fail "launch refused. Unlock the phone (and Settings > General > VPN & Device Management > Trust) then rerun."

  echo
  echo "app launched. The smoke report is NSLogged with prefix [chatter]:"
  echo "  xcrun devicectl device console --device $DEVICE | grep chatter"
}

# ---------------------------------------------------------- device control

# WARNING: `shot`/`drive` run `xcodebuild test`, which REINSTALLS the app, and a
# reinstall can replace its data container — the DB and any logged-in session go
# with it (the Keychain survives, so you get "Retrieved existing passphrase" over
# a brand-new empty DB, which reads as a bogus session-persistence bug). Never
# use these to verify persistence. For that, launch without reinstalling:
#   xcrun devicectl device process launch --device <id> --terminate-existing \
#     --console com.chatter.client
# CHATTER_ATTACH=1 makes the test photograph the running app instead of
# relaunching it, but the reinstall has already happened by then.
#
# Screenshot / tap the app on the phone via XCUITest. iOS has no adb: devicectl
# has no screenshot or tap subcommand, and libimobiledevice's screenshotr
# service is unreachable on iOS 17+ ("Invalid service" — it moved behind the
# RemoteXPC tunnel). XCUITest is the one route needing neither sudo nor extra
# deps. Screenshots come back as test attachments in the result bundle.
uitest() {
  local derived="$ROOT/target-ios/DerivedData"
  local res="$ROOT/target-ios/ui.xcresult"
  local shots="$ROOT/target-ios/shots"
  rm -rf "$res" "$shots"

  # Regenerate only when project.yml actually moved. Rewriting the .xcodeproj
  # makes an open Xcode reload and lose its run destination, which fights a
  # human doing the one GUI step (Cmd-U) that mints the xctrunner profile.
  if [ "$ROOT/ios/project.yml" -nt "$ROOT/ios/Chatter.xcodeproj/project.pbxproj" ]; then
    say "xcodegen (project.yml changed)"
    ( cd "$ROOT/ios" && xcodegen generate )
  fi

  # Same pairing as ios(): the cargo feature and the ObjC define must move
  # together, or the test rebuilds the app as the smoke harness and screenshots
  # that instead of the UI.
  local defs=()
  case "${CHATTER_IOS_FEATURES:-}" in
    *ui*) defs=(GCC_PREPROCESSOR_DEFINITIONS='$(inherited) CHATTER_RUST_UI=1') ;;
  esac

  say "xcuitest"
  # TEST_RUNNER_* must be in xcodebuild's OWN environment, not a build setting:
  # xcodebuild copies them (prefix stripped) into the runner process. Passed as
  # `KEY=value xcodebuild …` they only ever reached build scripts, so the test
  # silently saw no steps and just screenshotted the launch screen.
  TEST_RUNNER_CHATTER_DUMP="${CHATTER_DUMP:-}" \
  TEST_RUNNER_CHATTER_ATTACH="${CHATTER_ATTACH:-}" \
  TEST_RUNNER_CHATTER_STEPS="${CHATTER_STEPS:-}" \
  TEST_RUNNER_CHATTER_SETTLE="${CHATTER_SETTLE:-}" \
  xcodebuild test -project "$ROOT/ios/Chatter.xcodeproj" -scheme Chatter \
    -destination "platform=iOS,id=$DEVICE" \
    -derivedDataPath "$derived" -resultBundlePath "$res" \
    -allowProvisioningUpdates \
    ${defs+"${defs[@]}"} \
    ${CHATTER_SERVER_URL:+CHATTER_SERVER_URL="$CHATTER_SERVER_URL"} \
    2>&1 | grep -E "Testing started|Test Case|error:|BUILD|TEST" | tail -12

  say "screenshots"
  xcrun xcresulttool export attachments --path "$res" --output-path "$shots" >/dev/null 2>&1
  find "$shots" -name '*.png' -o -name '*.heic' | head -5
  [ -n "$(find "$shots" -name '*.png' -o -name '*.heic' 2>/dev/null)" ] \
    || echo "no attachment exported — check $res"
}

case "${1:-doctor}" in
  doctor) doctor ;;
  mac)    mac ;;
  ios)    ios ;;
  all)    mac; ios ;;
  shot)   uitest ;;
  tap)    [ $# -eq 3 ] || fail "usage: ship.sh tap <x> <y>   (normalized 0..1)"
          CHATTER_STEPS="$2,$3" uitest ;;
  # Several steps in one app session — the only way to fill a form, since each
  # uitest run relaunches the app and drops whatever the last one typed.
  drive)  [ $# -eq 2 ] || fail "usage: ship.sh drive '0.5,0.4 | wait:10 | 0.5,0.8'"
          CHATTER_STEPS="$2" uitest ;;
  *)      sed -n '2,16p' "$0"; exit 1 ;;
esac
