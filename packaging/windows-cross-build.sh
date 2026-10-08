#!/bin/sh
# MinGW cross-build used by Dockerfile.windows.
#
# Not the production Windows artifact: --features windows-cross has no
# LiveKit. Calls are built by build-windows.ps1 on native MSVC.
# dev-trust-root is intentionally absent. The four production pins must
# already be in the environment (docker run -e).
set -eu

echo "=== Chatter Windows Cross-Compilation ==="
echo "Target: x86_64-pc-windows-gnu (no LiveKit)"

missing=""
for v in \
    CHATTER_TLS_ROOT_CA_PEM \
    CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX \
    CHATTER_KT_TREE_HEAD_PUBLIC_KEY_HEX \
    CHATTER_KT_VRF_PUBLIC_KEY_HEX
do
    if [ -z "$(printenv "$v" 2>/dev/null || true)" ]; then
        missing="$missing $v"
    fi
done
if [ -n "$missing" ]; then
    echo "FATAL: production pins unset:$missing"
    echo "Pass them with docker run -e. Do not add --features dev-trust-root."
    exit 1
fi

export RUSTC_WRAPPER=
export PKG_CONFIG_PATH_x86_64_pc_windows_gnu="${GST_ROOT_DIR}/lib/pkgconfig"
export PKG_CONFIG_SYSROOT_DIR_x86_64_pc_windows_gnu="${GST_ROOT_DIR}"
export OPENSSL_DIR_x86_64_pc_windows_gnu="${GST_ROOT_DIR}"
export OPENSSL_INCLUDE_DIR_x86_64_pc_windows_gnu="${GST_ROOT_DIR}/include"
export OPENSSL_LIB_DIR_x86_64_pc_windows_gnu="${GST_ROOT_DIR}/lib"
export OPENSSL_STATIC_x86_64_pc_windows_gnu=1
export CFLAGS_x86_64_pc_windows_gnu="-I${GST_ROOT_DIR}/include"

find "${GST_ROOT_DIR}/lib/pkgconfig" -name '*.pc' -exec \
    sed -i "s|^prefix=.*|prefix=${GST_ROOT_DIR}|" {} +

if ! cargo build --release --target x86_64-pc-windows-gnu \
    --no-default-features --features windows-cross > /tmp/build.log 2>&1; then
    echo "=== Build Failed ==="
    tail -100 /tmp/build.log
    exit 1
fi
cat /tmp/build.log

exe="target/x86_64-pc-windows-gnu/release/chatter-ui.exe"
if [ ! -f "$exe" ]; then
    echo "=== Build Failed ==="
    tail -100 /tmp/build.log
    exit 1
fi

echo "=== Build Successful ==="
mkdir -p /output
cp "$exe" /output/chatter.exe
if [ -d "${GST_ROOT_DIR}/bin" ]; then
    cp "${GST_ROOT_DIR}/bin/"*.dll /output/ \
        || echo "warn: no DLLs copied from ${GST_ROOT_DIR}/bin"
fi
for name in libgcc_s_seh-1.dll libstdc++-6.dll libwinpthread-1.dll; do
    found="$(find /usr/lib/gcc/x86_64-w64-mingw32 /usr/x86_64-w64-mingw32 \
        -name "$name" -print -quit 2>/dev/null || true)"
    if [ -n "$found" ]; then
        cp "$found" /output/
    else
        echo "warn: $name not found; chatter.exe may not start"
    fi
done
echo "Output: /output/chatter.exe and runtime DLLs. windows-cross has no LiveKit."
ls -lh /output/
