//! Property-based tests for crypto round-trips and tamper resistance.
//!
//! Uses the `proptest` crate to drive randomized inputs (sizes, indices, byte
//! payloads) and check invariants that should hold for *every* such input —
//! e.g. encrypt/decrypt round-trips, anti-malleability, and parser robustness.
//!
//! All tests use only `chatter-core`'s public API surface.
//!
//! Cases per test are capped at 100 (see `ProptestConfig::with_cases(100)`)
//! so total runtime stays well under the 60 s budget on a workstation.
//!
//! Determinism: `proptest` derives its RNG from the `PROPTEST_SEED` env var
//! when set, otherwise from a fresh entropy seed printed in failure output —
//! the failing seed is also stored under `proptest-regressions/` so flakes can
//! be replayed.

use chatter_core::crypto::AttachmentCrypto;
use chatter_core::crypto::envelope_crypto::decrypt_envelope;
use chatter_core::error::CryptoError;

use proptest::prelude::*;

// =============================================================================
// Test 1: Attachment encrypt/decrypt round-trip
// =============================================================================
//
// Property: for any plaintext byte string of length 1..=100_000,
//   decrypt(encrypt(p).ciphertext, key, digest, len) == p
//
// Bounds rationale: 1 byte minimum (we have a separate `test_roundtrip_empty`
// integration test for the empty case; proptest shrinking on `0..=N` would
// repeatedly hit the well-tested empty input). Upper bound 100_000 keeps each
// case under a few ms of AES-256-CBC + HMAC work.

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_attachment_roundtrip(plaintext in proptest::collection::vec(any::<u8>(), 1..=100_000)) {
        let encrypted = AttachmentCrypto::encrypt(&plaintext)
            .expect("encrypt must succeed for any plaintext in [1, 100_000] bytes");

        let decrypted = AttachmentCrypto::decrypt(
            &encrypted.ciphertext,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        )
        .expect("decrypt must succeed on the matching ciphertext/key/digest");

        prop_assert_eq!(decrypted, plaintext);
    }
}

// =============================================================================
// Test 2: Sealed sender certificate tampering — SKIPPED
// =============================================================================
//
// The intended property: build a libsignal `SenderCertificate`, mutate one
// byte of its serialization, assert `validate_trust_root(&cert)` returns
// false.
//
// Why this is skipped:
//   * `validate_trust_root` (in `chatter_core::crypto::signal::sealed_sender`)
//     hardcodes the production trust-root public key from `config.rs`. The
//     matching trust-root **private** key is not embedded in the binary —
//     it lives only on the production server.
//   * To exercise the "tampering causes false" branch we'd need to *first*
//     produce a certificate that returns `true`. We cannot, because we
//     can't sign a `ServerCertificate` with the real trust-root private key.
//   * Any cert we synthesize with a freshly-generated trust-root private
//     key returns `false` *before* any tampering, making the property test
//     degenerate (true → false has no signal because it's already false).
//
// Reaching libsignal's `SenderCertificate::validate(&our_trust_root, t)`
// directly would test libsignal, not chatter-core. The task constraints
// forbid widening chatter-core's public API just to test it, and the
// `pub(crate)` accessors aren't relevant here either.
//
// Coverage of the underlying property is provided elsewhere:
//   * `crates/chatter-core/tests/integration_sealed_sender.rs` exercises
//     the real cert flow end-to-end against a test server.
//   * `crates/chatter-core/tests/security_key_lifecycle.rs` covers the
//     `CachedSenderCertificate` lifecycle without needing a valid trust
//     root.
//   * libsignal's own `rust/protocol/tests/sealed_sender.rs` proptest-style
//     tampering tests live upstream.

// =============================================================================
// Test 3: Attachment ciphertext bit-flip (anti-malleability)
// =============================================================================
//
// Property: for any plaintext + any ciphertext byte index + any bit index,
//   decrypt(flipped_ciphertext, key, original_digest, len)  must NOT return
//   Ok(p) — it must return Err(...).  In particular, integrity verification
//   must catch the tamper *before* releasing any plaintext bytes.
//
// Note: we do NOT recompute the digest after the flip. The `digest` field
// is the SHA-256 of the *original* ciphertext, which is what a relaying
// server would have committed to. A tampered ciphertext must therefore
// fail the digest check (or, if the attacker also recomputes the digest,
// the inner HMAC check — but here we test the canonical "flip ciphertext,
// keep digest" attacker model).

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_attachment_ciphertext_bitflip_rejected(
        plaintext in proptest::collection::vec(any::<u8>(), 1..=4096),
        byte_idx in any::<usize>(),
        bit_idx in 0u8..8,
    ) {
        let encrypted = AttachmentCrypto::encrypt(&plaintext)
            .expect("encrypt must succeed");

        // Project the random byte index into the ciphertext range.
        let idx = byte_idx % encrypted.ciphertext.len();

        let mut tampered = encrypted.ciphertext.clone();
        tampered[idx] ^= 1u8 << bit_idx;

        // Sanity: flip actually changed the ciphertext.
        prop_assert_ne!(&tampered, &encrypted.ciphertext);

        let result = AttachmentCrypto::decrypt(
            &tampered,
            &encrypted.key,
            &encrypted.digest,
            encrypted.plaintext_size,
        );

        // Must be an error — never silently producing garbage as Ok.
        prop_assert!(
            result.is_err(),
            "bit-flip at byte {} bit {} silently decrypted; result={:?}",
            idx, bit_idx, result
        );

        // And the error should be one of the integrity failures, not e.g.
        // a length error (we kept the length intact).
        match result {
            Err(CryptoError::AttachmentDigestMismatch)
            | Err(CryptoError::AttachmentMacMismatch)
            | Err(CryptoError::AttachmentDecryptionFailed(_)) => {}
            other => prop_assert!(
                false,
                "expected integrity error, got {:?}",
                other
            ),
        }
    }
}

// =============================================================================
// Test 4: Envelope parser robustness
// =============================================================================
//
// Property: for any byte slice of length 0..=10_000 and any 32-byte
// "private key", `decrypt_envelope(slice, &key, None)` MUST NOT panic.
// It must always return Ok or Err — never abort the process.
//
// The chance of random bytes successfully decrypting under a random ECDH
// shared secret is cryptographically negligible, so in practice we always
// expect Err — but the property under test is *no panics*, not "always
// errors". `decrypt_envelope` is the closest analogue in our public API to
// a "prekey bundle / envelope parser" — it's the entry point that ingests
// untrusted bytes off the wire for the NDSS '21 mailbox transport.
//
// Note: `EciesMessage::from_bytes` (also public) is too lenient to be a
// useful target — it only length-checks and slices, with no chance to
// panic. `decrypt_envelope` exercises real curve-point decompression,
// HKDF, and AEAD code paths.

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn prop_envelope_parser_never_panics(
        envelope in proptest::collection::vec(any::<u8>(), 0..=10_000),
        recipient_seed in proptest::array::uniform32(any::<u8>()),
    ) {
        // `std::panic::catch_unwind` is only useful if the function might
        // panic on logic bugs — proptest itself will surface a panic as a
        // failure with the offending seed.  We rely on proptest's failure
        // reporting and just call the function: any panic = test failure.
        let result = decrypt_envelope(&envelope, &recipient_seed, None);

        // Either Ok (astronomically unlikely) or Err — both fine. The
        // property is "no panic", which is enforced by control reaching
        // this assertion at all.
        match result {
            Ok(_) | Err(_) => {}
        }
    }
}
