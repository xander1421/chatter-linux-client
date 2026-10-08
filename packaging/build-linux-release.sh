#!/usr/bin/env bash
# Production x86_64 Linux system-runtime archive, built here, never an arbitrary binary.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
die() { echo "FATAL: $*" >&2; exit 1; }
[ "$#" = 0 ] || die 'No extra cargo arguments are accepted'
[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] || die 'Build on x86_64 Linux'
for flag in DEV_TRUST_ROOT DEV_AUTO_REGISTER; do
    [ "${!flag:-0}" = 0 ] || die "$flag must be 0"
done
for v in CHATTER_SERVER_URL CHATTER_TLS_ROOT_CA_PEM CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX \
    CHATTER_KT_TREE_HEAD_PUBLIC_KEY_HEX CHATTER_KT_VRF_PUBLIC_KEY_HEX CHATTER_LINUX_VERSION; do
    [ -n "${!v:-}" ] || die "$v is required (existing provisioned production inputs)"
done
python3 - <<'CHECK'
import ipaddress, os, re, sys
from urllib.parse import urlsplit
def fail(message): sys.exit('FATAL: ' + message)
url = urlsplit(os.environ['CHATTER_SERVER_URL'])
host = (url.hostname or '').lower().rstrip('.')
try: port = url.port
except ValueError: fail('invalid server port')
if url.scheme != 'https' or url.username or url.password or url.query or url.fragment or url.path not in ('', '/'):
    fail('CHATTER_SERVER_URL must be a public HTTPS RPC origin')
if not host or '.' not in host or any(host == x or host.endswith('.' + x) for x in ('example.com', 'example.org', 'example.net', 'localhost', 'local', 'invalid', 'test')):
    fail('public provisioned DNS hostname required')
try: ipaddress.ip_address(host)
except ValueError: pass
else: fail('DNS hostname required, not an IP literal')
if not re.fullmatch(r'[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?', host): fail('invalid DNS hostname')
dev = ('ca49d96aa6417d724b4531dfc6cbdc8e291b5231caac088a3092de5120be6833',
       '88ad412806054a7534e1b9deb42ef249bd7c92709f5bebf1d6be7b0e40926f72',
       '48af975feaba33ea110d4ab0c8f7a38c5c7122db185ff9499b81e5e74da6ec47')
for key in ('CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX', 'CHATTER_KT_TREE_HEAD_PUBLIC_KEY_HEX', 'CHATTER_KT_VRF_PUBLIC_KEY_HEX'):
    value = os.environ[key]
    if not re.fullmatch(r'[0-9a-fA-F]{64}', value) or value.lower() in dev: fail(key + ' must be a non-development 32-byte public pin')
if not re.fullmatch(r'[0-9][A-Za-z0-9._+-]{0,63}', os.environ['CHATTER_LINUX_VERSION']): fail('invalid CHATTER_LINUX_VERSION')
CHECK
# Parse the supplied public CA; never fetch trust material or read private keys.
printf '%s\n' "$CHATTER_TLS_ROOT_CA_PEM" | openssl x509 -noout -checkend 0 >/dev/null || die 'CA is invalid or expired'
ca_info=$(printf '%s\n' "$CHATTER_TLS_ROOT_CA_PEM" | openssl x509 -noout -text)
[[ "$ca_info" == *'CA:TRUE'* ]] || die 'TLS anchor must be a CA certificate'
# Cargo env rustflags replace target config flags. Preserve its linker choices
# explicitly and replace native CPU selection with the baseline x86_64 ISA.
[ -z "${RUSTFLAGS:-}" ] && [ -z "${CARGO_ENCODED_RUSTFLAGS:-}" ] || die 'Unset custom Rust flags for this controlled release build'
unset CARGO_ENCODED_RUSTFLAGS
export RUSTFLAGS='-C link-arg=-fuse-ld=mold -C link-arg=-Wl,--allow-multiple-definition -C target-cpu=x86-64'
export CFLAGS="${CFLAGS:-} -march=x86-64 -mtune=generic"
export CXXFLAGS="${CXXFLAGS:-} -march=x86-64 -mtune=generic"
# Dedicated output avoids packaging the developer installer's old release binary.
export CARGO_TARGET_DIR="$ROOT/target/linux-production"
features='desktop,jarvis,mesh,quic'
if [ -n "${CHATTER_ONION_ADDR:-}" ]; then
    [[ "$CHATTER_ONION_ADDR" =~ ^[a-z2-7]{56}\.onion$ ]] || die 'CHATTER_ONION_ADDR must be the provisioned v3 onion hostname'
    features+=',onion'
fi
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
printf 'version=%s\ntarget=x86_64-unknown-linux-gnu\ncpu=x86-64\nfeatures=%s\nruntime=system GStreamer (not bundled)\n' "$CHATTER_LINUX_VERSION" "$features" > "$STAGE/$NAME/BUILD.txt"
tar -C "$STAGE" -czf "$OUT/$NAME.tar.gz" "$NAME"
(cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
echo "Package: $OUT/$NAME.tar.gz (requires system runtime; see packaging/README.md)"
