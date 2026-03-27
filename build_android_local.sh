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
# Build the APK locally
cargo apk build --release --no-default-features --features mobile -p chatter-ui

echo "---------------------------------------------------"
echo "Build complete! Your APK should be located at:"
echo "./target/release/apk/chatter_ui.apk"
echo "---------------------------------------------------"
