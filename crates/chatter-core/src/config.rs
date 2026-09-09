//! Application configuration.
//!
//! Contains hardcoded security-critical values that must be baked into the binary.
//!
//! # Trust Root Key Security Model
//!
//! Following Signal's security model, the sealed-sender trust root public key
//! is baked into the binary. A sender certificate that does not verify under it
//! is worthless, so a malicious server cannot mint a certificate binding an
//! arbitrary UUID to an identity key it controls.
//!
//! Selection is by an EXPLICIT build feature that DEFAULTS TO PRODUCTION. It is
//! deliberately NOT keyed on `debug_assertions`: `android/build-android.sh` is
//! the only Android build path in the tree and builds debug on purpose, and
//! `[profile.release-dev]` forces `debug-assertions` back on — under the old
//! scheme both silently shipped the insecure key.
//!
//! - **Default** (`cargo build`, `cargo build --release`, everything CI does):
//!   the production trust root, taken at compile time from the
//!   `CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX` environment variable (64 hex chars =
//!   32 raw bytes, no `0x05` prefix). If that variable is unset the binary
//!   builds but PANICS the moment anything needs the trust root — which is
//!   during `SignalService` construction, i.e. at startup.
//! - **`--features dev-trust-root`**: the development trust root matching the
//!   local docker-compose Vault. Its private key is committed in plaintext at
//!   `deploy/vault/init-and-unseal.sh`, so this build trusts certificates
//!   anyone can forge. Local development only.
//!
//! Configuring production:
//!
//! ```sh
//! CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX=<64 hex chars> cargo build --release
//! ```
//!
//! The value is the public half of the Curve25519 keypair the server signs
//! sealed-sender certificates with — `public_key` under
//! `sealed-sender/signing-keys/current` in Vault, base64 there, hex here. A
//! malformed value, or one equal to the dev key, fails the BUILD.

use libsignal_protocol::PublicKey;

// ─── Sealed sender trust root ──────────────────────────────────────────────

/// Public half of the development keypair provisioned by
/// `deploy/vault/init-and-unseal.sh` when `VAULT_GENERATE_KEYS` is not `true`.
///
/// Base64: `yknZaqZBfXJLRTHfxsvcjikbUjHKrAiKMJLeUSC+aDM=`
///
/// The matching PRIVATE key is committed next to it in that script, so this key
/// authenticates nobody. Kept here so the production path can refuse to equal
/// it, and so `--features dev-trust-root` can select it on purpose.
const DEV_TRUST_ROOT_BYTES: [u8; 32] = [
    0xca, 0x49, 0xd9, 0x6a, 0xa6, 0x41, 0x7d, 0x72,
    0x4b, 0x45, 0x31, 0xdf, 0xc6, 0xcb, 0xdc, 0x8e,
    0x29, 0x1b, 0x52, 0x31, 0xca, 0xac, 0x08, 0x8a,
    0x30, 0x92, 0xde, 0x51, 0x20, 0xbe, 0x68, 0x33,
];

/// The trust root a SHIPPABLE artifact of this build would pin — the cdylib,
/// a binary, anything a downstream crate links. `None` means none is
/// configured, which is a startup panic, not a silent downgrade.
///
/// This is the value the security property is about, and the one
/// `dev_trust_root_requires_explicit_feature` asserts on.
#[cfg(feature = "dev-trust-root")]
const SHIPPED_TRUST_ROOT_BYTES: Option<[u8; 32]> = Some(DEV_TRUST_ROOT_BYTES);

/// The trust root a SHIPPABLE artifact of this build would pin. See above.
#[cfg(not(feature = "dev-trust-root"))]
const SHIPPED_TRUST_ROOT_BYTES: Option<[u8; 32]> =
    match option_env!("CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX") {
        Some(hex) => Some(decode_hex32(hex)),
        None => None,
    };

/// The trust root this build actually uses.
///
/// Identical to [`SHIPPED_TRUST_ROOT_BYTES`] except that chatter-core's own
/// unit-test harness falls back to the dev key, so running `cargo test` does
/// not require every developer to export a production key. `cfg(test)` is set
/// ONLY when rustc compiles this crate's test harness — never for the cdylib,
/// a binary, or a downstream crate — so unlike `debug_assertions` it cannot
/// leak an insecure key into anything shippable.
pub const TRUST_ROOT_PUBLIC_KEY_BYTES: Option<[u8; 32]> = match SHIPPED_TRUST_ROOT_BYTES {
    Some(configured) => Some(configured),
    None if cfg!(test) || cfg!(feature = "testing") => Some(DEV_TRUST_ROOT_BYTES),
    None => None,
};

/// Fail the BUILD when no trust root is configured, not the first session.
///
/// Without this the build succeeds and `TRUST_ROOT_PUBLIC_KEY` panics on first
/// use inside a `LazyLock` — so an unprovisioned artifact ships, installs, and
/// dies on the user's machine instead of on the machine that built it. The KT
/// tree-head anchor below already fails at compile time; this makes the two
/// pinned keys behave the same way, which is the whole point of having one
/// provisioning step for both.
///
/// `cfg(test)` / `feature = "testing"` are excluded because those harnesses
/// legitimately fall back to the dev key above.
#[cfg(all(not(feature = "dev-trust-root"), not(test), not(feature = "testing")))]
const _: () = {
    assert!(
        TRUST_ROOT_PUBLIC_KEY_BYTES.is_some(),
        "no sealed-sender trust root is baked into this build.\n\n\
         Sender certificates could not be verified against anything, so this \
         binary would panic at startup.\n\n\
         Build with the production key:\n\n  \
         CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX=<64 hex chars> cargo build --release\n\n\
         Generate it with `make provision-keys` in the server repo.\n\n\
         For local development only, build with --features dev-trust-root."
    );
};

/// GRA-249, enforced at build time: a production build must not be handed the
/// publicly-known development key.
#[cfg(not(feature = "dev-trust-root"))]
const _: () = {
    match SHIPPED_TRUST_ROOT_BYTES {
        Some(configured) => {
            let mut i = 0;
            let mut same = true;
            while i < 32 {
                if configured[i] != DEV_TRUST_ROOT_BYTES[i] {
                    same = false;
                }
                i += 1;
            }
            assert!(
                !same,
                "CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX is the publicly-known development \
                 trust root, whose private key is committed in deploy/vault/init-and-unseal.sh. \
                 Use --features dev-trust-root if that is what you meant."
            );
        }
        None => {}
    }
};

/// Decode 64 hex characters into 32 bytes at compile time. A wrong length or a
/// non-hex character is a build failure, not a runtime surprise.
#[cfg(not(feature = "dev-trust-root"))]
const fn decode_hex32(hex: &str) -> [u8; 32] {
    let bytes = hex.as_bytes();
    assert!(
        bytes.len() == 64,
        "CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX must be exactly 64 hex characters (32 bytes)"
    );
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = hex_nibble(bytes[i * 2]) * 16 + hex_nibble(bytes[i * 2 + 1]);
        i += 1;
    }
    out
}

#[cfg(not(feature = "dev-trust-root"))]
const fn hex_nibble(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => panic!("CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX contains a non-hex character"),
    }
}

/// What the owner must do when the trust root was never provisioned. This is
/// the whole failure mode: the binary links, then dies here.
const MISSING_TRUST_ROOT: &str = "\n\
    ==============================================================================\n\
    FATAL: no sealed-sender trust root is baked into this build.\n\
    \n\
    Sender certificates cannot be verified, so every sealed-sender message would\n\
    have to be either rejected or trusted blindly. Refusing to run.\n\
    \n\
    To fix, rebuild with the production trust root public key:\n\
    \n\
      CHATTER_TRUST_ROOT_PUBLIC_KEY_HEX=<64 hex chars> cargo build --release\n\
    \n\
    That value is the public half of the Curve25519 keypair the SERVER signs\n\
    sealed-sender certificates with: the `public_key` field of\n\
    `sealed-sender/signing-keys/current` in Vault (base64 there, hex here).\n\
    Provision it with VAULT_GENERATE_KEYS=true deploy/vault/init-and-unseal.sh,\n\
    which generates a fresh random keypair, then read the public half back out.\n\
    Client and server MUST be built/provisioned from the same keypair.\n\
    \n\
    For local development against the docker-compose Vault, build with\n\
    `--features dev-trust-root` instead. That key's PRIVATE half is committed in\n\
    this repository and anyone can forge certificates under it — never ship it.\n\
    ==============================================================================\n";

/// Parsed trust root public key for use with libsignal.
///
/// Panics on first use if the build has no trust root configured — see
/// [`MISSING_TRUST_ROOT`]. First use is `SignalService::new`, so this surfaces
/// at startup rather than mid-conversation.
pub static TRUST_ROOT_PUBLIC_KEY: std::sync::LazyLock<PublicKey> = std::sync::LazyLock::new(|| {
    let bytes = match TRUST_ROOT_PUBLIC_KEY_BYTES {
        Some(bytes) => bytes,
        None => panic!("{}", MISSING_TRUST_ROOT),
    };
    // libsignal's 33-byte wire format: 0x05 (Curve25519) || key.
    let mut serialized = [0u8; 33];
    serialized[0] = 0x05;
    let mut i = 0;
    while i < 32 {
        serialized[i + 1] = bytes[i];
        i += 1;
    }
    PublicKey::deserialize(&serialized)
        .expect("trust root is not a valid Curve25519 point - check the configured key")
});

// ─── TLS root CA (private-PKI transport pin, §8) ───────────────────────────
//
// Signal's transport model, ported: the client trusts ONLY this CA for its own
// hostname-TLS endpoints — the direct gRPC channel and the anonymous HTTP path
// over Tor — and NEVER the operating-system / webpki root store. A rogue or
// mis-issued *public* CA certificate therefore cannot MITM the transport, the
// way it can with system-root trust. Onion endpoints are self-authenticating by
// their `.onion` address (the address IS the key) and carry no TLS certificate,
// so they need nothing here.
//
// Unlike the sealed-sender trust root and the KT tree head (32-byte curve keys),
// the anchor is a full X.509 CA certificate: we pin the CA, not a leaf SPKI —
// exactly as Signal does — so the server can rotate its TLS leaf freely as long
// as the new leaf still chains to this CA. No client rebuild per rotation, and
// no SPKI-leaf pin that would break on one. Same provisioning story as the two
// pinned keys: a dev build embeds a committed placeholder; a non-dev build must
// supply the real CA at build time or fail to compile.

/// Test-only placeholder CA (committed, INSECURE, and keyless — its private
/// key was never saved, so it can sign nothing). Used ONLY by `cargo test` /
/// `--features testing`, which never open a real TLS connection and just need
/// a non-empty trust store to satisfy the type. There is no plaintext server
/// mode to fall back to anymore — every real build, dev-trust-root included,
/// needs a CA whose private key actually exists.
const DEV_TLS_ROOT_CA_PEM: &str = include_str!("../assets/dev_tls_root_ca.pem");

/// The TLS CA a SHIPPABLE artifact of this build would pin. `None` means none
/// is configured — a build failure below, not a silent fall-through to system
/// roots. Same for every build, `dev-trust-root` included: TLS is required in
/// every environment now, so there is no dev/prod split here the way the
/// sealed-sender trust root and KT tree head still have one (those two stay
/// insecure-by-default under `dev-trust-root`; this one cannot, because the
/// keyless placeholder CA can't authenticate anything and the server it would
/// need to front — a plaintext dev loop — no longer exists).
const SHIPPED_TLS_ROOT_CA_PEM: Option<&str> = option_env!("CHATTER_TLS_ROOT_CA_PEM");

/// The TLS trust anchor this build actually uses. `cfg(test)` / `feature =
/// "testing"` fall back to the dev placeholder so `cargo test` needs no
/// provisioning — identical posture to the sealed-sender trust root above, and
/// safe for the same reason (`cfg(test)` is set only for this crate's own test
/// harness, never for a shippable artifact).
pub const TLS_ROOT_CA_PEM: Option<&str> = match SHIPPED_TLS_ROOT_CA_PEM {
    // An empty `CHATTER_TLS_ROOT_CA_PEM` reaches `option_env!` as `Some("")`,
    // which would satisfy an `is_some()` guard while baking in NO trust anchor —
    // every hostname-TLS connection would then fail at runtime. Treat empty as
    // unconfigured so the build-time guard below rejects it (or test/testing
    // falls back to the dev CA), never the user's first handshake.
    Some(pem) if !pem.is_empty() => Some(pem),
    _ if cfg!(test) || cfg!(feature = "testing") => Some(DEV_TLS_ROOT_CA_PEM),
    _ => None,
};

/// Fail the BUILD when no TLS CA is provisioned for a shippable artifact —
/// `dev-trust-root` included; TLS is mandatory in every environment, not just
/// production. Because the client trusts ONLY the pinned CA (never system
/// roots), an unprovisioned build would reject every hostname-TLS connection
/// at runtime — so, like the sealed-sender trust root, this must fail on the
/// machine that builds it, not the user's.
#[cfg(all(not(test), not(feature = "testing")))]
const _: () = {
    assert!(
        TLS_ROOT_CA_PEM.is_some(),
        "no TLS root CA is baked into this build.\n\n\
         The client trusts ONLY the pinned chatter CA for its gRPC and anonymous \
         HTTP endpoints (never the system roots), so without one every TLS \
         connection would fail at runtime. This applies to --features \
         dev-trust-root too -- there is no plaintext server mode to fall back \
         to; TLS is required in every environment.\n\n\
         For local development: run ./create-certs.sh, then build with\n\n  \
         CHATTER_TLS_ROOT_CA_PEM=\"$(cat ../certs/server.crt)\" cargo build --features dev-trust-root\n\n\
         For a shippable build, use your server's real CA certificate instead \
         of the local dev one, and drop --features dev-trust-root."
    );
};

// ─── Key Transparency tree-head signing key (pinned) ───────────────────────
//
// The KT server signs every SignedTreeHead with an Ed25519 key; the client
// pins its PUBLIC key here, exactly as it pins TRUST_ROOT_PUBLIC_KEY for
// sealed-sender certificates. A signed tree head whose signature does not
// verify under this key is worthless, so a malicious server cannot forge a
// tree head that commits to an identity key it controls.
//
// This is the raw 32-byte Ed25519 public key (NO 0x05 prefix — that prefix is a
// libsignal Curve25519 convention; KT tree heads are plain RFC-8032 Ed25519,
// matching the Go server's github.com/cloudflare/circl/sign/ed25519 signer).
//
// Development key (server logs it loudly at boot, service.go:41):
//   "Public key: 88ad412806054a7534e1b9deb42ef249bd7c92709f5bebf1d6be7b0e40926f72"
//
// PHASE 3 TODO (Vault prod signing key): production deployments must move the KT
// tree-head signing key into Vault and replace the release bytes below with the
// corresponding public key (mirroring the GRA-249 fix for TRUST_ROOT). Until
// then the server itself runs on the well-known dev key (CHATTER_KT_ALLOW_DEV_KEY),
// so both build profiles pin the same key.
/// The well-known DEVELOPMENT tree-head key. Its private half is derived from
/// `devSigningSeedPhrase` in the server's `signingkey.go` — a seed phrase in
/// source, so anyone with either repository can sign a tree head that this key
/// verifies. It is only ever pinned under the `dev-trust-root` feature.
const DEV_KT_TREE_HEAD_BYTES: [u8; 32] = [
    0x88, 0xad, 0x41, 0x28, 0x06, 0x05, 0x4a, 0x75,
    0x34, 0xe1, 0xb9, 0xde, 0xb4, 0x2e, 0xf2, 0x49,
    0xbd, 0x7c, 0x92, 0x70, 0x9f, 0x5b, 0xeb, 0xf1,
    0xd6, 0xbe, 0x7b, 0x0e, 0x40, 0x92, 0x6f, 0x72,
];

/// The pinned key-transparency tree-head signing key.
///
/// Sourced exactly like `SHIPPED_TRUST_ROOT_BYTES`, and for the same reason: a
/// key that can only be changed by editing source is a key nobody rotates. Set
/// `CHATTER_KT_TREE_HEAD_PUBLIC_KEY_HEX` (64 hex chars) at build time to pin a
/// real one.
///
/// The append-only half is wired now (2026-08-03, OPEN-ITEMS §3):
/// `services/signal/kt_anchor.rs` pins the highest tree head accepted so far
/// and only advances it through a verified `verify_consistency_proof`, on
/// every KT-verified send — `production_wiring.rs` fails if that caller
/// disappears. So the two checks are complementary and BOTH need a real key
/// here: against the dev key a forged head satisfies the inclusion proof and
/// the consistency proof alike, because the forger can sign every head in the
/// chain. A provisioned key is what makes either one mean anything.
#[cfg(feature = "dev-trust-root")]
pub const KT_TREE_HEAD_PUBLIC_KEY_BYTES: [u8; 32] = DEV_KT_TREE_HEAD_BYTES;

#[cfg(not(feature = "dev-trust-root"))]
pub const KT_TREE_HEAD_PUBLIC_KEY_BYTES: [u8; 32] =
    match option_env!("CHATTER_KT_TREE_HEAD_PUBLIC_KEY_HEX") {
        Some(hex) => decode_hex32(hex),
        // Test harnesses fall back to the dev key so `cargo test` does not
        // require every developer to export a production one.
        //
        // `cfg(test)` covers this crate's unit tests. It does NOT cover the
        // integration tests in `tests/`, which link chatter-core as an ordinary
        // dependency with `cfg(test)` unset — hence `feature = "testing"`, which
        // exists only to widen accessors for exactly those tests and is
        // documented in Cargo.toml as never to be enabled for an application
        // build (chatter-ui does not enable it).
        //
        // Neither is `debug_assertions`, which the Android build path sets.
        None if cfg!(test) || cfg!(feature = "testing") => DEV_KT_TREE_HEAD_BYTES,
        // No configured key. Falling back to the development key here is exactly
        // the defect this replaces, so the build stops instead.
        None => panic!(
            "no key-transparency tree-head key is baked into this build.\n\n\
             Signed tree heads cannot be verified against anything trustworthy.\n\n\
             Build with the production key:\n\n  \
             CHATTER_KT_TREE_HEAD_PUBLIC_KEY_HEX=<64 hex chars> cargo build --release\n\n\
             It is the public half of the Ed25519 key the KT server signs tree heads\n\
             with — see deploy/vault and the server's keytransparency signingkey.go.\n\n\
             For local development only, build with --features dev-trust-root."
        ),
    };

/// A non-dev build must not end up pinning the development tree-head key, even
/// if someone exports its hex by mistake. Mirrors the trust-root assertion.
#[cfg(all(not(feature = "dev-trust-root"), not(test), not(feature = "testing")))]
const _: () = {
    let mut i = 0;
    let mut same = true;
    while i < 32 {
        if KT_TREE_HEAD_PUBLIC_KEY_BYTES[i] != DEV_KT_TREE_HEAD_BYTES[i] {
            same = false;
        }
        i += 1;
    }
    assert!(
        !same,
        "CHATTER_KT_TREE_HEAD_PUBLIC_KEY_HEX is the publicly-known development \
         tree-head key, whose private half derives from a seed phrase in source. \
         Provision a real key or build with --features dev-trust-root."
    );
};

// ─── KT query-privacy VRF public key (Phase 3b) ────────────────────────────
//
// The KT log's sparse-Merkle leaf key is now VRF_sk(user_id) — an ECVRF-
// EDWARDS25519-SHA512-TAI output — instead of SHA-256(user_id). This hides the
// user_id -> leaf-index mapping from an auditor scanning the public log while
// keeping it committed and verifiable. The client PINS the server's VRF PUBLIC
// key here and verifies the `vrf_proof` in every GetProofResponse under it, so a
// malicious server cannot substitute a different VRF key (which would let it
// answer with an arbitrary leaf). This is the raw 32-byte Ed25519-form VRF
// public key.
//
// Development key: derived from the well-known dev VRF seed
// SHA-256("chatter-dev-kt-vrf-v1") (keytransparency.LoadVRFKey). The server logs
// "KT query-privacy VRF key loaded" with this public key at boot.
//
// The DEVELOPMENT VRF key, derived from SHA-256("chatter-dev-kt-vrf-v1").
const DEV_KT_VRF_BYTES: [u8; 32] = [
    0x48, 0xaf, 0x97, 0x5f, 0xea, 0xba, 0x33, 0xea,
    0x11, 0x0d, 0x4a, 0xb0, 0xc8, 0xf7, 0xa3, 0x8c,
    0x5c, 0x71, 0x22, 0xdb, 0x18, 0x5f, 0xf9, 0x49,
    0x9b, 0x81, 0xe5, 0xe7, 0x4d, 0xa6, 0xec, 0x47,
];

/// The pinned KT query-privacy VRF public key.
///
/// Provisionable via `CHATTER_KT_VRF_PUBLIC_KEY_HEX`, matching the other two
/// pinned keys. Until 2026-08-03 this was a bare constant with the dev value and
/// a "PHASE 3b TODO" — and the server-side half was unreachable too:
/// `CHATTER_KT_VRF_SEED` was read by `LoadVRFKey` but set nowhere in
/// docker-compose, `.env.example`, `scripts/` or `deploy/`. So the documented
/// production path existed on neither side. `make provision-keys` now generates
/// it and emits both halves.
///
/// CRYPTO-6: this used to be the one anchor of the four that failed OPEN on both
/// halves — a missing value silently pinned the dev key here, and the server
/// silently signed with the matching dev seed. The old justification (the VRF
/// key's secrecy buys QUERY PRIVACY, not integrity, so degraded unlinkability
/// should not stop a build) understates two real failures:
///
///   - a shipped client that pins the dev key against a *provisioned* server
///     rejects every VRF proof, so KT verifies nothing at all — silently; and
///   - a deploy where both halves fall back runs on a world-known key, and
///     anyone with this repository can compute `user_id -> leaf index` and
///     de-anonymise the entire log.
///
/// So it now fails closed exactly like the tree-head key and the TLS root: no
/// provisioned value, no build. `make provision-keys` emits both halves.
#[cfg(feature = "dev-trust-root")]
pub const KT_VRF_PUBLIC_KEY_BYTES: [u8; 32] = DEV_KT_VRF_BYTES;

#[cfg(not(feature = "dev-trust-root"))]
pub const KT_VRF_PUBLIC_KEY_BYTES: [u8; 32] =
    match option_env!("CHATTER_KT_VRF_PUBLIC_KEY_HEX") {
        Some(hex) => decode_hex32(hex),
        // Same harness carve-out as the tree-head key above: `cargo test` must
        // not require a provisioned key. Never true for an application build.
        None if cfg!(test) || cfg!(feature = "testing") => DEV_KT_VRF_BYTES,
        None => panic!(
            "no key-transparency VRF public key is baked into this build.\n\n\
             Every GetProof response carries a VRF proof binding the queried user_id\n\
             to the leaf index served. Pinned against the development key, that proof\n\
             either verifies nothing (dev server) or fails outright (real server), and\n\
             the log's user_id -> leaf-index mapping is public to anyone with this\n\
             repository.\n\n\
             Build with the production key:\n\n  \
             CHATTER_KT_VRF_PUBLIC_KEY_HEX=<64 hex chars> cargo build --release\n\n\
             It is printed by `make provision-keys` alongside the tree-head key.\n\n\
             For local development only, build with --features dev-trust-root."
        ),
    };

/// A non-dev build must not end up pinning the development VRF key either.
/// Mirrors the tree-head assertion above.
#[cfg(all(not(feature = "dev-trust-root"), not(test), not(feature = "testing")))]
const _: () = {
    let mut i = 0;
    let mut same = true;
    while i < 32 {
        if KT_VRF_PUBLIC_KEY_BYTES[i] != DEV_KT_VRF_BYTES[i] {
            same = false;
        }
        i += 1;
    }
    assert!(
        !same,
        "CHATTER_KT_VRF_PUBLIC_KEY_HEX is the publicly-known development VRF key, \
         whose private half derives from a seed phrase in source. Provision a real \
         key or build with --features dev-trust-root."
    );
};

// ─── Shared constants ──────────────────────────────────────────────────────

/// Trust root key ID (matches server's key_id in Vault).
/// Used for key rotation coordination.
pub const TRUST_ROOT_KEY_ID: u32 = 1;

/// Messages fetched per page when loading a conversation or paging older
/// history. Single source of truth so the fetch limit and the `has_more`
/// check (`len >= MESSAGE_PAGE_SIZE`) can't drift apart.
pub const MESSAGE_PAGE_SIZE: i32 = 50;

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD as BASE64, Engine};

    /// The DEV trust root's private key is committed in plaintext at
    /// `deploy/vault/init-and-unseal.sh`, so anyone can mint a sender
    /// certificate under it. Selecting it must require an explicit opt-in
    /// feature — a debug build must never silently become an insecure build.
    /// (`android/build-android.sh` is the only Android build path and builds
    /// debug on purpose; `[profile.release-dev]` used to force
    /// `debug-assertions` back on for the same reason.)
    #[test]
    #[cfg(not(feature = "dev-trust-root"))]
    fn dev_trust_root_requires_explicit_feature() {
        let dev_public = BASE64
            .decode("yknZaqZBfXJLRTHfxsvcjikbUjHKrAiKMJLeUSC+aDM=")
            .expect("known-good base64");
        // SHIPPED_, not TRUST_ROOT_PUBLIC_KEY_BYTES: the latter carries the
        // cfg(test) fallback, which by construction only exists inside this
        // very harness. The property under test is what a cdylib/binary pins.
        assert_ne!(
            SHIPPED_TRUST_ROOT_BYTES.as_ref().map(|b| &b[..]),
            Some(&dev_public[..]),
            "build selected the publicly-known dev trust root without the \
             `dev-trust-root` feature — sender certificates are forgeable"
        );
    }

    /// Opting in must actually get you the key the local dev Vault signs with,
    /// otherwise `--features dev-trust-root` is useless and someone will reach
    /// for `debug_assertions` again.
    #[test]
    #[cfg(feature = "dev-trust-root")]
    fn dev_trust_root_feature_selects_the_dev_vault_key() {
        let expected = "yknZaqZBfXJLRTHfxsvcjikbUjHKrAiKMJLeUSC+aDM=";
        let actual = BASE64.encode(SHIPPED_TRUST_ROOT_BYTES.expect("feature pins a key"));
        assert_eq!(actual, expected, "must match the docker-compose Vault key");
        // And it must parse — this is the LazyLock's only non-panicking path.
        let _ = TRUST_ROOT_PUBLIC_KEY.serialize();
    }

    /// Whatever the build configured, it must be a valid Curve25519 point in
    /// libsignal's 33-byte format. Unconfigured builds have nothing to check.
    #[test]
    fn configured_trust_root_is_a_valid_curve_point() {
        let Some(bytes) = TRUST_ROOT_PUBLIC_KEY_BYTES else {
            return;
        };
        let mut serialized = [0u8; 33];
        serialized[0] = 0x05;
        serialized[1..].copy_from_slice(&bytes);
        assert!(
            PublicKey::deserialize(&serialized).is_ok(),
            "Trust root key must be a valid Curve25519 point"
        );
    }

    /// The pinned TLS CA (§8) must be present in a test/dev build and be a
    /// structurally valid PEM certificate. Because the client trusts ONLY this
    /// CA (never system roots) on the gRPC and anon-HTTP paths, a missing,
    /// empty, or renamed asset would leave it trusting nothing and failing every
    /// handshake — catch that on this machine, not on a user's first connection.
    #[test]
    fn tls_root_ca_is_a_wellformed_pem_certificate() {
        let pem = TLS_ROOT_CA_PEM.expect("test/dev builds pin the committed dev CA");
        assert!(
            pem.contains("-----BEGIN CERTIFICATE-----"),
            "TLS_ROOT_CA_PEM is not a PEM certificate"
        );
        // Decode the DER body and confirm it opens with an ASN.1 SEQUENCE tag,
        // i.e. it is a real X.509 certificate rather than an empty/corrupt
        // placeholder. Cheap structural check, no X.509 parser dependency.
        let body: String = pem
            .lines()
            .skip_while(|l| !l.contains("BEGIN CERTIFICATE"))
            .skip(1)
            .take_while(|l| !l.contains("END CERTIFICATE"))
            .collect();
        let der = BASE64
            .decode(body.trim())
            .expect("CA PEM body must be valid base64");
        assert!(der.len() > 64, "DER certificate implausibly small");
        assert_eq!(
            der[0], 0x30,
            "DER certificate must begin with an ASN.1 SEQUENCE tag"
        );
    }
}
