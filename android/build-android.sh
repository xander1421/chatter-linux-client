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

# GStreamer helper libs (pcre2/gmodule/ffi/orc/iconv/z) still link on mobile.
export GSTREAMER_ROOT_ANDROID="${GSTREAMER_ROOT_ANDROID:-$HOME/gst-android}"
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_LIBDIR="$GSTREAMER_ROOT_ANDROID/arm64/lib/pkgconfig"
export PKG_CONFIG_PATH="$GSTREAMER_ROOT_ANDROID/arm64/lib/pkgconfig"

# SQLCipher crypto provider = webrtc's bundled BoringSSL.
#
# SQLCipher (rusqlite `bundled-sqlcipher`) needs an OpenSSL-API libcrypto. We
# point it at the BoringSSL baked into LiveKit's libwebrtc.a instead of a
# separate OpenSSL — so the whole cdylib has exactly ONE libcrypto (BoringSSL).
# A separate OpenSSL would collide with webrtc's BoringSSL on the same symbol
# names (different ABI) and corrupt the heap in webrtc's network thread on calls.
# SQLCipher 4.5.7 is BoringSSL-compatible (uses EVP_CIPHER_nid + OBJ_nid2sn, not
# the BoringSSL-removed EVP_CIPHER_name). libwebrtc.a exports every symbol it
# needs (EVP_*, HMAC, PKCS5_PBKDF2_HMAC, RAND_bytes — verified).
setup_boringssl_for_sqlcipher() {
    local webrtc_a webrtc_inc stage
    webrtc_a="$(find "$PROJECT_ROOT/target" -name libwebrtc.a -path '*android-arm64-release*' 2>/dev/null | head -1)"
    webrtc_inc="$(find "$PROJECT_ROOT/target" -type d -path '*android-arm64-release*/third_party/boringssl/src/include' 2>/dev/null | head -1)"
    if [ -z "$webrtc_a" ] || [ -z "$webrtc_inc" ]; then
        echo "ERROR: webrtc BoringSSL artifacts not found under target/."
        echo "       Run a normal cargo-ndk build once so webrtc-sys downloads libwebrtc.a, then re-run."
        echo "       (looked for libwebrtc.a + third_party/boringssl/src/include)"
        exit 1
    fi
    stage="$PROJECT_ROOT/target/boringssl-sqlcipher"
    mkdir -p "$stage/lib"
    # -lcrypto resolves to webrtc's BoringSSL. Duplicate of the main libwebrtc.a
    # link is collapsed by -Wl,--allow-multiple-definition (set in build.rs).
    cp -f "$webrtc_a" "$stage/lib/libcrypto.a"
    export OPENSSL_DIR="$stage"
    export OPENSSL_LIB_DIR="$stage/lib"
    export OPENSSL_INCLUDE_DIR="$webrtc_inc"
    export OPENSSL_STATIC=1
    echo ">> SQLCipher → BoringSSL: -lcrypto=$stage/lib/libcrypto.a  headers=$webrtc_inc"
}
setup_boringssl_for_sqlcipher

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

# Note: LiveKit's webrtc Java classes (livekit.org.webrtc / jni_zero) are pulled
# straight from the webrtc-sys download under target/ by app/build.gradle.kts
# (fileTree) — produced by the cargo-ndk step above, so no copy step is needed.

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
    echo ">> installing + setting reverse tunnels (50051 gRPC, 9000 RustFS storage)…"
    # 50051 = ConnectRPC server. 9000 = RustFS/S3 — attachment uploads hit a
    # presigned URL at 127.0.0.1:9000, which on-device is the phone's localhost,
    # so it must be tunnelled to the host too or attachment sends fail.
    "$ADB" reverse tcp:50051 tcp:50051 || true
    "$ADB" reverse tcp:9000 tcp:9000 || true
    # 7880 = LiveKit signaling (WS), 7881 = LiveKit ICE/TCP media fallback.
    # (UDP media won't tunnel over adb; for real calls use the host LAN IP.)
    "$ADB" reverse tcp:7880 tcp:7880 || true
    "$ADB" reverse tcp:7881 tcp:7881 || true
    "$ADB" install -r -d "$APK"
    "$ADB" shell monkey -p com.chatter.client -c android.intent.category.LAUNCHER 1 >/dev/null 2>&1 || true
    echo ">> launched com.chatter.client"
fi
