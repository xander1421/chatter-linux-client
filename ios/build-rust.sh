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

# Cargo builds host tools as well as the iOS library. Xcode's SDK environment
# must not make host C dependencies compile against iPhone headers (or no
# sysroot after SDKROOT is cleared). Give cc-rs a separate SDK for each side.
HOST_SDK="$(xcrun --sdk macosx --show-sdk-path)"
IOS_SDK="$(xcrun --sdk "${PLATFORM_NAME:-iphoneos}" --show-sdk-path)"
CLANG="$(xcrun --find clang)"
CLANGXX="$(xcrun --find clang++)"

# A stale desktop OpenSSL path also breaks the iOS sysroot.
env -u OPENSSL_DIR -u OPENSSL_LIB_DIR -u OPENSSL_INCLUDE_DIR \
    -u SDKROOT -u CPATH -u LIBRARY_PATH \
    HOST_CC="$CLANG" HOST_CXX="$CLANGXX" \
    HOST_CFLAGS="-isysroot $HOST_SDK" HOST_CXXFLAGS="-isysroot $HOST_SDK" \
    TARGET_CC="$CLANG" TARGET_CXX="$CLANGXX" \
    TARGET_CFLAGS="-isysroot $IOS_SDK" TARGET_CXXFLAGS="-isysroot $IOS_SDK" \
    MACOSX_DEPLOYMENT_TARGET="${MACOSX_DEPLOYMENT_TARGET:-11.0}" \
    CARGO_TARGET_DIR="$TARGET_DIR" \
    cargo build -p chatter-ios "${ARGS[@]}"

mkdir -p "$(dirname "$0")/build/lib"
cp "$TARGET_DIR/$RUST_TARGET/$PROFILE/libchatter_ios.a" \
   "$(dirname "$0")/build/lib/libchatter_ios.a"
