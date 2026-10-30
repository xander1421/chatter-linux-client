#!/usr/bin/env bash
# Refuse production client builds whose public inputs are absent, development
# values or local placeholders. Shared by build-linux-release.sh and
# android/build-android.sh RELEASE=1. Reads only the environment and the
# supplied public CA; never fetches trust material or reads a private key.
#
# Usage: check-release-inputs.sh <server-url>
# Environment: CHATTER_TLS_ROOT_CA_PEM, the three public key pins, and the
# optional CHATTER_NTFY_URL / CHATTER_ONION_ADDR.
set -euo pipefail
die() { echo "FATAL: $*" >&2; exit 1; }
[ "$#" = 1 ] && [ -n "$1" ] || die 'usage: check-release-inputs.sh <server-url>'
for flag in DEV_TRUST_ROOT DEV_AUTO_REGISTER; do
    [ "${!flag:-0}" = 0 ] || die "$flag must be 0"
done
for v in CHATTER_TLS_ROOT_CA_PEM CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX \
    CHATTER_KT_TREE_HEAD_PUBLIC_KEY_HEX CHATTER_KT_VRF_PUBLIC_KEY_HEX \
    CHATTER_RELEASE_UPDATE_PUBLIC_KEY_HEX; do
    [ -n "${!v:-}" ] || die "$v is required (existing provisioned production inputs)"
done
CHECK_SERVER_URL="$1" python3 - <<'CHECK'
import ipaddress, os, re, sys
from urllib.parse import urlsplit
def fail(message): sys.exit('FATAL: ' + message)
def public_host(url, what):
    host = (url.hostname or '').lower().rstrip('.')
    try: url.port
    except ValueError: fail('invalid port in ' + what)
    if url.username or url.password or url.query or url.fragment:
        fail(what + ' must not carry credentials, query or fragment')
    if not host or '.' not in host or any(host == x or host.endswith('.' + x) for x in ('example.com', 'example.org', 'example.net', 'localhost', 'local', 'invalid', 'test', 'internal', 'lan', 'home.arpa')):
        fail(what + ': public provisioned DNS hostname required')
    try: ipaddress.ip_address(host)
    except ValueError: pass
    else: fail(what + ': DNS hostname required, not an IP literal')
    if not re.fullmatch(r'[a-z0-9](?:[a-z0-9.-]*[a-z0-9])?', host): fail(what + ': invalid DNS hostname')
    return host
url = urlsplit(os.environ['CHECK_SERVER_URL'])
if url.scheme != 'https' or url.path not in ('', '/'):
    fail('server URL must be a public HTTPS RPC origin')
public_host(url, 'server URL')
ntfy = os.environ.get('CHATTER_NTFY_URL', '')
if ntfy:
    url = urlsplit(ntfy)
    if (url.hostname or '').endswith('.onion'):
        if not re.fullmatch(r'[a-z2-7]{56}\.onion', url.hostname) or url.scheme not in ('http', 'https'):
            fail('CHATTER_NTFY_URL onion host must be a v3 onion hostname')
    elif url.scheme != 'https':
        fail('CHATTER_NTFY_URL must be HTTPS (or a v3 onion)')
    else:
        public_host(url, 'CHATTER_NTFY_URL')
onion = os.environ.get('CHATTER_ONION_ADDR', '')
if onion and not re.fullmatch(r'[a-z2-7]{56}\.onion', onion):
    fail('CHATTER_ONION_ADDR must be the provisioned v3 onion hostname')
# Committed development public keys (scripts/anchors.sh DEV_*_HEX); forgeable by anyone.
dev = ('ca49d96aa6417d724b4531dfc6cbdc8e291b5231caac088a3092de5120be6833',
       '88ad412806054a7534e1b9deb42ef249bd7c92709f5bebf1d6be7b0e40926f72',
       '48af975feaba33ea110d4ab0c8f7a38c5c7122db185ff9499b81e5e74da6ec47',
       'ca2efc2c4186cdcdf29fabe4f33975dd4672bc2e09469a42595a864fca39795a')
for key in ('CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX', 'CHATTER_KT_TREE_HEAD_PUBLIC_KEY_HEX', 'CHATTER_KT_VRF_PUBLIC_KEY_HEX', 'CHATTER_RELEASE_UPDATE_PUBLIC_KEY_HEX'):
    value = os.environ[key]
    if not re.fullmatch(r'[0-9a-fA-F]{64}', value) or value.lower() in dev: fail(key + ' must be a non-development 32-byte public pin')
CHECK
printf '%s\n' "$CHATTER_TLS_ROOT_CA_PEM" | openssl x509 -noout -checkend 0 >/dev/null || die 'CA is invalid or expired'
ca_info=$(printf '%s\n' "$CHATTER_TLS_ROOT_CA_PEM" | openssl x509 -noout -text)
[[ "$ca_info" == *'CA:TRUE'* ]] || die 'TLS anchor must be a CA certificate'
# create-certs.sh mints a per-checkout localhost CA under this subject; its key
# sits unprotected in a developer's certs/ (provision-trust-root.sh DEV_CA_MARKER).
[[ "$ca_info" != *'Chatter Local Development CA'* ]] || die 'CHATTER_TLS_ROOT_CA_PEM is a create-certs.sh development CA'
# The committed development CA (config.rs DEV_TLS_ROOT_CA_PEM) is public too.
dev_ca="$(dirname "$0")/../crates/chatter-core/assets/dev_tls_root_ca.pem"
fp() { openssl x509 -noout -fingerprint -sha256; }
[ "$(printf '%s\n' "$CHATTER_TLS_ROOT_CA_PEM" | fp)" != "$(fp < "$dev_ca")" ] || die 'CHATTER_TLS_ROOT_CA_PEM is the committed development CA'
