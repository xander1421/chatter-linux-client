#!/bin/bash
# Build the Slint/Rust client as an Android APK via the GameActivity Gradle host.
#
# Pipeline: cargo-ndk builds libchatter_ui.so (arm64, debug) -> copied into
# app/src/main/jniLibs -> Gradle assembles + signs the APK.
#
# DEBUG build on purpose: debug_assertions pins the DEV sealed-sender trust root
# (config.rs) which matches the local dev Vault. A --release build pins the
# production trust root and cannot validate dev-server sender certs.
#
# Usage: ./build-android.sh [install]
#   install -> also adb-install to the connected device and set the 50051 tunnel.
set -euo pipefail
cd "$(dirname "$0")"
PROJECT_ROOT="$(cd .. && pwd)"

# ── Toolchain / cross-compile env ───────────────────────────────────────────
export ANDROID_HOME="${ANDROID_HOME:-$HOME/Android/Sdk}"
export ANDROID_SDK_ROOT="$ANDROID_HOME"
export ANDROID_NDK_HOME="${ANDROID_NDK_HOME:-$ANDROID_HOME/ndk/28.2.13676358}"
export ANDROID_PLATFORM=android-34
export ANDROID_API_LEVEL=34
# i-slint-backend-android-activity build.rs probes for android.jar via android-build.
export ANDROID_JAR="$ANDROID_HOME/platforms/android-34/android.jar"

# GStreamer (statically linked into the cdylib) + OpenSSL for SQLCipher.
export GSTREAMER_ROOT_ANDROID="${GSTREAMER_ROOT_ANDROID:-$HOME/gst-android}"
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_LIBDIR="$GSTREAMER_ROOT_ANDROID/arm64/lib/pkgconfig"
export PKG_CONFIG_PATH="$GSTREAMER_ROOT_ANDROID/arm64/lib/pkgconfig"
export OPENSSL_DIR="$GSTREAMER_ROOT_ANDROID/arm64"
export OPENSSL_INCLUDE_DIR="$GSTREAMER_ROOT_ANDROID/arm64/include"
export OPENSSL_LIB_DIR="$GSTREAMER_ROOT_ANDROID/arm64/lib"
export OPENSSL_STATIC=1

# NDK r28 ships only llvm-* tools; openssl-sys (vendored) wants the gnu-named ones.
NDK_BIN="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin"
export RANLIB_aarch64_linux_android="$NDK_BIN/llvm-ranlib"
export NM_aarch64_linux_android="$NDK_BIN/llvm-nm"
export STRIP_aarch64_linux_android="$NDK_BIN/llvm-strip"
export CARGO_TARGET_AARCH64_LINUX_ANDROID_RANLIB="$NDK_BIN/llvm-ranlib"

# 16 KB page alignment for Android 15+ devices (NDK r28 default, set explicitly).
export RUSTFLAGS="${RUSTFLAGS:-} -C link-arg=-Wl,-z,max-page-size=16384"

# ── 1. Build the cdylib ─────────────────────────────────────────────────────
echo ">> cargo-ndk: building libchatter_ui.so (arm64-v8a, debug)…"
( cd "$PROJECT_ROOT" && cargo ndk -t arm64-v8a build -p chatter-ui \
    --no-default-features --features mobile --lib )

# ── 2. Stage the .so into the Gradle module ─────────────────────────────────
JNI_DIR="app/src/main/jniLibs/arm64-v8a"
mkdir -p "$JNI_DIR"
cp "$PROJECT_ROOT/target/aarch64-linux-android/debug/libchatter_ui.so" "$JNI_DIR/"
echo ">> staged $(du -h "$JNI_DIR/libchatter_ui.so" | cut -f1) cdylib"

# ── 3. Assemble the APK ─────────────────────────────────────────────────────
echo ">> gradle: assembleDebug…"
JAVA_HOME="${JAVA_HOME:-/opt/android-studio/jbr}" ./gradlew :app:assembleDebug --no-daemon

APK="app/build/outputs/apk/debug/app-debug.apk"
echo "------------------------------------------------------------"
echo "Build complete: $APK"
echo "------------------------------------------------------------"

# ── 4. Optional: install + tunnel ───────────────────────────────────────────
if [ "${1:-}" = "install" ]; then
    ADB="$ANDROID_HOME/platform-tools/adb"
    echo ">> installing + setting 50051 reverse tunnel…"
    "$ADB" reverse tcp:50051 tcp:50051 || true
    "$ADB" install -r -d "$APK"
    "$ADB" shell monkey -p com.chatter.client -c android.intent.category.LAUNCHER 1 >/dev/null 2>&1 || true
    echo ">> launched com.chatter.client"
fi
