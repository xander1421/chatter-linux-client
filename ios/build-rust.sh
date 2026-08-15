#!/bin/bash
# Builds the Rust staticlib Xcode links against.
#
# CARGO_TARGET_DIR is pinned to target-ios on purpose: a parallel macOS
# workflow owns ./target and cargo takes a lock per target dir.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
TARGET_DIR="$REPO/target-ios"
PROFILE="${RUST_PROFILE:-debug}"

case "${PLATFORM_NAME:-iphoneos}" in
  iphonesimulator) RUST_TARGET=aarch64-apple-ios-sim ;;
  *)               RUST_TARGET=aarch64-apple-ios ;;
esac

# One array, always non-empty — macOS ships bash 3.2, where `"${empty[@]}"`
# trips `set -u`.
ARGS=(--target "$RUST_TARGET" --manifest-path "$REPO/Cargo.toml")
[ "$PROFILE" = "release" ] && ARGS+=(--release)
[ -n "${CHATTER_IOS_FEATURES:-}" ] && ARGS+=(--features "$CHATTER_IOS_FEATURES")

# Xcode injects an SDK-specific toolchain env that confuses cargo's cc probing,
# and a stale OPENSSL_DIR from the desktop shell breaks the iOS sysroot.
env -u OPENSSL_DIR -u OPENSSL_LIB_DIR -u OPENSSL_INCLUDE_DIR \
    -u SDKROOT -u CPATH -u LIBRARY_PATH \
    CARGO_TARGET_DIR="$TARGET_DIR" \
    cargo build -p chatter-ios "${ARGS[@]}"

mkdir -p "$(dirname "$0")/build/lib"
cp "$TARGET_DIR/$RUST_TARGET/$PROFILE/libchatter_ios.a" \
   "$(dirname "$0")/build/lib/libchatter_ios.a"
