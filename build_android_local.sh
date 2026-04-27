#!/bin/bash

# Ensure we're in the project root
cd "$(dirname "$0")"

echo "Setting up local Android build environment..."

# 1. Add Rust targets if not already present
rustup target add aarch64-linux-android armv7-linux-androideabi i686-linux-android x86_64-linux-android

# 2. Set up environment variables (also in .bashrc now)
export ANDROID_HOME=$HOME/Android/Sdk
export ANDROID_NDK_HOME=$HOME/Android/Sdk/ndk/28.2.13676358
export GSTREAMER_ROOT_ANDROID=$HOME/gst-android
export JAVA_HOME=/opt/android-studio/jbr
export ANDROID_PLATFORM=android-34

# 2.1 Set up GStreamer cross-compilation variables for pkg-config
# Only setting for arm64 (aarch64) for now as it's our primary target
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_LIBDIR=$GSTREAMER_ROOT_ANDROID/arm64/lib/pkgconfig
export PKG_CONFIG_PATH=$GSTREAMER_ROOT_ANDROID/arm64/lib/pkgconfig

# 2.2 Set up OpenSSL variables for SQLCipher (rusqlite)
# GStreamer Android SDK includes OpenSSL which we can reuse
export OPENSSL_DIR=$GSTREAMER_ROOT_ANDROID/arm64
export OPENSSL_INCLUDE_DIR=$GSTREAMER_ROOT_ANDROID/arm64/include
export OPENSSL_LIB_DIR=$GSTREAMER_ROOT_ANDROID/arm64/lib
export OPENSSL_STATIC=1

# 2.3 NDK binutils shims — openssl-sys (vendored) calls `aarch64-linux-android-{ranlib,nm,strip}`
# but NDK r28 only ships `llvm-*` tools. Point the per-target env vars at llvm-*.
NDK_BIN=$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin
export RANLIB_aarch64_linux_android=$NDK_BIN/llvm-ranlib
export NM_aarch64_linux_android=$NDK_BIN/llvm-nm
export STRIP_aarch64_linux_android=$NDK_BIN/llvm-strip
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RANLIB=$NDK_BIN/llvm-ranlib

# openssl-src (used by openssl's `vendored` feature) invokes
# `aarch64-linux-android-{ranlib,nm,ar,strip}` directly from its Makefile.
# NDK r28 consolidates these into `llvm-*`, so we expose symlinks via a shim
# dir prepended to PATH.
SHIM_DIR=$(mktemp -d)
ln -sf "$NDK_BIN/llvm-ranlib" "$SHIM_DIR/aarch64-linux-android-ranlib"
ln -sf "$NDK_BIN/llvm-nm"     "$SHIM_DIR/aarch64-linux-android-nm"
ln -sf "$NDK_BIN/llvm-ar"     "$SHIM_DIR/aarch64-linux-android-ar"
ln -sf "$NDK_BIN/llvm-strip"  "$SHIM_DIR/aarch64-linux-android-strip"
export PATH="$SHIM_DIR:$PATH"

# 3. Verify paths
if [ ! -d "$ANDROID_NDK_HOME" ]; then
    echo "ERROR: ANDROID_NDK_HOME not found at $ANDROID_NDK_HOME"
    exit 1
fi

if [ ! -d "$GSTREAMER_ROOT_ANDROID" ]; then
    echo "ERROR: GSTREAMER_ROOT_ANDROID not found at $GSTREAMER_ROOT_ANDROID"
    exit 1
fi

echo "Starting local Android Build for ARM64..."
# Release build — signing configured via [package.metadata.android.signing.release]
# in crates/chatter-ui/Cargo.toml (points at Android debug keystore for local dev).
cargo apk build --release --no-default-features --features mobile -p chatter-ui

APK_DIR="${CARGO_TARGET_DIR:-./target}/release/apk"
echo "---------------------------------------------------"
echo "Build complete! APK at: $APK_DIR/chatter_ui.apk"
echo "---------------------------------------------------"
