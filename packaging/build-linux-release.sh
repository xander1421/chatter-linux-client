#!/usr/bin/env bash
# Production x86_64 Linux system-runtime archive, built here, never an arbitrary binary.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
die() { echo "FATAL: $*" >&2; exit 1; }
[ "$#" = 0 ] || die 'No extra cargo arguments are accepted'
[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] || die 'Build on x86_64 Linux'
for v in CHATTER_SERVER_URL CHATTER_LINUX_VERSION CHATTER_LINUX_VERSION_CODE; do
    [ -n "${!v:-}" ] || die "$v is required (existing provisioned production inputs)"
done
[[ "$CHATTER_LINUX_VERSION" =~ ^[0-9][A-Za-z0-9._+-]{0,63}$ ]] || die 'invalid CHATTER_LINUX_VERSION'
[[ "$CHATTER_LINUX_VERSION_CODE" =~ ^[1-9][0-9]{0,9}$ ]] &&
    [ "$CHATTER_LINUX_VERSION_CODE" -le 2100000000 ] || die 'invalid CHATTER_LINUX_VERSION_CODE'
"$ROOT/packaging/check-release-inputs.sh" "$CHATTER_SERVER_URL"
# Cargo env rustflags replace target config flags. Preserve its linker choices
# explicitly and replace native CPU selection with the baseline x86_64 ISA.
[ -z "${RUSTFLAGS:-}" ] && [ -z "${CARGO_ENCODED_RUSTFLAGS:-}" ] || die 'Unset custom Rust flags for this controlled release build'
unset CARGO_ENCODED_RUSTFLAGS
export RUSTFLAGS='-C link-arg=-fuse-ld=mold -C link-arg=-Wl,--allow-multiple-definition -C target-cpu=x86-64'
export CFLAGS="${CFLAGS:-} -march=x86-64 -mtune=generic"
export CXXFLAGS="${CXXFLAGS:-} -march=x86-64 -mtune=generic"
# Dedicated output avoids packaging the developer installer's old release binary.
export CARGO_TARGET_DIR="$ROOT/target/linux-production"
# onion is in chatter-ui's default set (secret chats ride Tor only); keep it
# here too. Without a pinned CHATTER_ONION_ADDR secret chats and Shield fail closed.
features='desktop,jarvis,mesh,quic,onion'
[ -n "${CHATTER_ONION_ADDR:-}" ] || echo 'WARNING: no CHATTER_ONION_ADDR: secret chats and Shield will be unavailable in this build' >&2
cargo build --locked --release --target x86_64-unknown-linux-gnu -p chatter-ui \
    --bin chatter-ui --no-default-features --features "$features"
BIN="$CARGO_TARGET_DIR/x86_64-unknown-linux-gnu/release/chatter-ui"
"$ROOT/packaging/check-linux-runtime.sh" "$BIN"
OUT="$ROOT/target/linux-packages"
mkdir -p "$OUT"
STAGE=$(mktemp -d "$OUT/.stage.XXXXXX")
trap 'rm -rf "$STAGE"' EXIT
NAME="chatter-${CHATTER_LINUX_VERSION}-linux-x86_64-system-runtime"
mkdir -p "$STAGE/$NAME/bin" "$STAGE/$NAME/assets"
install -m 0755 "$BIN" "$STAGE/$NAME/bin/chatter-ui"
install -m 0755 "$ROOT/packaging/check-linux-runtime.sh" "$STAGE/$NAME/"
cp "$ROOT/packaging/README.md" "$STAGE/$NAME/README.md"
cp "$ROOT/assets/chatter-linux.desktop" "$STAGE/$NAME/assets/"
cp "$ROOT/assets/"icon-*.png "$STAGE/$NAME/assets/"
printf 'version=%s\nversion_code=%s\ntarget=x86_64-unknown-linux-gnu\ncpu=x86-64\nfeatures=%s\nruntime=system GStreamer (not bundled)\n' "$CHATTER_LINUX_VERSION" "$CHATTER_LINUX_VERSION_CODE" "$features" > "$STAGE/$NAME/BUILD.txt"
tar -C "$STAGE" -czf "$OUT/$NAME.tar.gz" "$NAME"
(cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
echo "Package: $OUT/$NAME.tar.gz (requires system runtime; see packaging/README.md)"
