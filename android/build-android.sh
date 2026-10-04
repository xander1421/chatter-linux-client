#!/bin/bash
# Build the Slint/Rust client as an Android APK via the GameActivity Gradle host.
#
# Pipeline: cargo-ndk builds libchatter_ui.so (arm64, debug) -> copied into
# app/src/main/jniLibs -> Gradle assembles + signs the APK.
#
# DEBUG build on purpose: fast iteration, symbols, no LTO. The sealed-sender
# trust root is NO LONGER tied to debug_assertions — it is selected by the
# explicit `dev-trust-root` cargo feature below, which this script enables by
# default because it targets the local dev Vault. See config.rs.
#
# Usage: ./build-android.sh [install]
#   install -> also adb-install to the connected device and set the 50051 tunnel.
#   MULTI_INSTANCE=1 -> build base, .b and .c debug packages in one Gradle run.
set -euo pipefail
cd "$(dirname "$0")"
PROJECT_ROOT="$(cd .. && pwd)"
# Cargo honours CARGO_TARGET_DIR (a worktree sharing one build cache); so must we.
TARGET_DIR="${CARGO_TARGET_DIR:-$PROJECT_ROOT/target}"

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
    webrtc_a="$(find "$TARGET_DIR" -name libwebrtc.a -path '*android-arm64-release*' 2>/dev/null | head -1)"
    webrtc_inc="$(find "$TARGET_DIR" -type d -path '*android-arm64-release*/third_party/boringssl/src/include' 2>/dev/null | head -1)"
    if [ -z "$webrtc_a" ] || [ -z "$webrtc_inc" ]; then
        echo "ERROR: webrtc BoringSSL artifacts not found under target/."
        echo "       Run a normal cargo-ndk build once so webrtc-sys downloads libwebrtc.a, then re-run."
        echo "       (looked for libwebrtc.a + third_party/boringssl/src/include)"
        exit 1
    fi
    stage="$TARGET_DIR/boringssl-sqlcipher"
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
echo ">> cargo-ndk: building libchatter_ui.so (arm64-v8a)…"
# Mesh is included by default; MESH=0 omits the Bluetooth transport.
MESH_FEATURE=""
if [ "${MESH:-1}" = "1" ]; then
    MESH_FEATURE=",mesh"
    echo ">> mesh enabled (BLE radio + Kotlin bridge)"
fi

# Pinned keys. This script targets the local docker-compose stack, whose Vault
# signs with the committed dev keypairs, so it opts into the dev keys
# EXPLICITLY. All three development keys are forgeable by anyone
# with a copy of this repository.
#
# The `dev-trust-root` feature selects the dev value for all three key pins:
#   - the sealed-sender trust root (forged sender certificates = impersonation)
#   - the KT tree-head key         (forged signed tree heads = KT defeated)
#   - the KT VRF key               (public seed defeats query privacy)
#
# For a real build, export DEV_TRUST_ROOT=0 and all three hex values. Resolve them with
# `scripts/anchors.sh exec` after provisioning in the server repo.
# The TLS CA is the fourth anchor and is required in both modes.
TRUST_ROOT_FEATURE=""
if [ "${DEV_TRUST_ROOT:-1}" = "1" ]; then
    TRUST_ROOT_FEATURE=",dev-trust-root"
    echo ">> !! DEV pinned keys (trust root + KT tree head + VRF) — forgeable, local dev only"
else
    missing=""
    [ -z "${CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX:-}" ] && missing="$missing CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX"
    [ -z "${CHATTER_KT_TREE_HEAD_PUBLIC_KEY_HEX:-}" ] && missing="$missing CHATTER_KT_TREE_HEAD_PUBLIC_KEY_HEX"
    [ -z "${CHATTER_KT_VRF_PUBLIC_KEY_HEX:-}" ] && missing="$missing CHATTER_KT_VRF_PUBLIC_KEY_HEX"
    if [ -n "$missing" ]; then
        echo "FATAL: DEV_TRUST_ROOT=0 but unset:$missing"
        echo
        echo "All three are compile-time pins. Without them the cargo build fails"
        echo "(config.rs refuses to fall back to a dev key), so this stops here"
        echo "with a better message than a const-eval panic."
        echo
        echo "Provision keys:  ./scripts/provision-trust-root.sh   (server repo)"
        exit 1
    fi
    echo ">> production pinned keys from the environment"
fi

# The TLS root CA is the FOURTH pin, separate from DEV_TRUST_ROOT above (which
# covers the sealed-sender, KT tree-head and VRF keys) -- the server has no plaintext mode,
# so every build needs one now, dev-trust-root included. Local dev: point it
# at create-certs.sh's cert. Real build: your server's actual CA.
if [ -z "${CHATTER_TLS_ROOT_CA_PEM:-}" ]; then
    echo "FATAL: CHATTER_TLS_ROOT_CA_PEM is unset."
    echo
    echo "The client trusts ONLY this pinned CA for TLS (never system roots), and"
    echo "the server has no plaintext fallback, so this is a compile-time pin now"
    echo "regardless of DEV_TRUST_ROOT."
    echo
    echo "Local dev:  CHATTER_TLS_ROOT_CA_PEM=\"\$(cat ../../certs/ca.crt)\" $0"
    echo "  (run ./create-certs.sh in the server repo first if that file doesn't exist)"
    exit 1
fi

# QUIC/HTTP3 transport -- probed and preferred over TCP/HTTP2, falls back
# automatically when unreachable. Unconditional now, same as TLS: it's the
# transport, not an opt-in layered on top of one.
QUIC_FEATURE=",quic"

# Tor v3 onion transport for anon mode (Shield). CHATTER_ONION_ADDR is the
# server's .onion, pinned into the build (config.rs PINNED_ONION_ADDR) — read
# it from the server's ONION_DATA_DIR/hs/hostname after starting it with
# ONION_ENABLED=true. Without it Shield cannot hide the network location and
# says so.
ONION_FEATURE=""
if [ -n "${CHATTER_ONION_ADDR:-}" ]; then
    ONION_FEATURE=",onion"
    echo ">> onion transport pinned: $CHATTER_ONION_ADDR"
else
    echo ">> no CHATTER_ONION_ADDR: Shield will not hide the network location"
fi

# Auto-register a throwaway dev account on first launch, with fixed credentials
# that are in this repository in plain sight (chatter-ui `dev_auto_register`).
#
# This is a compile-time cargo feature, NOT a runtime OS check. It used to be
# `cfg!(target_os = "android")` in lib.rs, which is true for every Android build
# — so a RELEASE APK auto-registered with those credentials too. This script only
# ever produces a debug APK against the local docker-compose stack, so it opts in
# by default; any release Android pipeline added later must simply not pass this
# feature, and the literals will not be in the binary.
#
# Set DEV_AUTO_REGISTER=0 to build the debug APK with the normal register screen.
DEV_AUTO_REGISTER_FEATURE=""
if [ "${DEV_AUTO_REGISTER:-1}" = "1" ]; then
    DEV_AUTO_REGISTER_FEATURE=",dev-auto-register"
    echo ">> !! dev auto-register enabled — first launch self-registers with public credentials"
fi

# RELEASE=1: optimized Rust (profile.release), a signed assembleRelease, and a
# refusal to ship any dev key or the dev auto-register credentials. Signing
# needs CHATTER_KEYSTORE, CHATTER_KEYSTORE_PASSWORD, CHATTER_KEY_ALIAS and
# CHATTER_KEY_PASSWORD (read by app/build.gradle.kts).
RUST_PROFILE=debug; CARGO_PROFILE_FLAG=""; GRADLE_TASK=assembleDebug; APK="app/build/outputs/apk/debug/app-debug.apk"
GRADLE_ARGS=()
if [ "${MULTI_INSTANCE:-0}" = "1" ]; then
    [ "${RELEASE:-0}" != "1" ] || { echo "FATAL: MULTI_INSTANCE=1 is debug-only"; exit 1; }
    GRADLE_ARGS+=(-PmultiInstance=true)
    APK="app/build/outputs/apk/base/debug/app-base-debug.apk"
fi
if [ "${RELEASE:-0}" = "1" ]; then
    if [ -n "$TRUST_ROOT_FEATURE" ] || [ -n "$DEV_AUTO_REGISTER_FEATURE" ]; then
        echo "FATAL: RELEASE=1 needs DEV_TRUST_ROOT=0 and DEV_AUTO_REGISTER=0"
        exit 1
    fi
    [ -z "${CHATTER_KT_VRF_PUBLIC_KEY_HEX:-}" ] && { echo "FATAL: RELEASE=1 needs CHATTER_KT_VRF_PUBLIC_KEY_HEX"; exit 1; }
    for v in CHATTER_KEYSTORE CHATTER_KEYSTORE_PASSWORD CHATTER_KEY_ALIAS CHATTER_KEY_PASSWORD; do
        [ -z "${!v:-}" ] && { echo "FATAL: RELEASE=1 needs $v"; exit 1; }
    done
    RUST_PROFILE=release; CARGO_PROFILE_FLAG="--release"; GRADLE_TASK=assembleRelease
    APK="app/build/outputs/apk/release/app-release.apk"
    echo ">> RELEASE build: production keys, signed"
fi

( cd "$PROJECT_ROOT" && cargo ndk -t arm64-v8a build -p chatter-ui $CARGO_PROFILE_FLAG \
    --no-default-features --features "mobile${MESH_FEATURE}${TRUST_ROOT_FEATURE}${QUIC_FEATURE}${ONION_FEATURE}${DEV_AUTO_REGISTER_FEATURE}" --lib )

# ── 2. Stage the .so into the Gradle module ─────────────────────────────────
JNI_DIR="app/src/main/jniLibs/arm64-v8a"
mkdir -p "$JNI_DIR"
cp "$TARGET_DIR/aarch64-linux-android/$RUST_PROFILE/libchatter_ui.so" "$JNI_DIR/"
# Strip only the staged copy; target/ retains symbols for native diagnostics.
# --strip-debug leaves the enormous .symtab/.strtab in place. --strip-unneeded
# removes them while preserving dynamic exports (including WebRTC JNI).
"$NDK_BIN/llvm-strip" --strip-unneeded "$JNI_DIR/libchatter_ui.so"
echo ">> staged $(du -h "$JNI_DIR/libchatter_ui.so" | cut -f1) cdylib"

# Note: LiveKit's webrtc Java classes (livekit.org.webrtc / jni_zero) are pulled
# straight from the webrtc-sys download under target/ by app/build.gradle.kts
# (fileTree) — produced by the cargo-ndk step above, so no copy step is needed.

# ── 3. Assemble the APK ─────────────────────────────────────────────────────
echo ">> gradle: $GRADLE_TASK…"
# Gradle 8.10 needs a supported JDK; Android Studio may bundle a newer one.
JAVA_HOME="${JAVA_HOME:-/usr/lib/jvm/java-17-openjdk}" ./gradlew :app:$GRADLE_TASK "${GRADLE_ARGS[@]}" --no-daemon
[ -f "$APK" ] || { echo "FATAL: $APK missing (unsigned release?)"; exit 1; }
echo "------------------------------------------------------------"
echo "Build complete: $APK"
if [ "${MULTI_INSTANCE:-0}" = "1" ]; then
    for variant in b c; do
        variant_apk="app/build/outputs/apk/$variant/debug/app-$variant-debug.apk"
        [ -f "$variant_apk" ] || { echo "FATAL: $variant_apk missing"; exit 1; }
        echo "Build complete: $variant_apk (com.chatter.client.$variant)"
    done
fi
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
