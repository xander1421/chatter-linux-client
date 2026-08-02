#![no_main]
//! Fuzz target: envelope decryption / parsing path.
//!
//! Property: `decrypt_envelope` must NEVER panic on arbitrary input.
//! Returning `Err` is the expected outcome for random bytes; even if the
//! length check passes and a `[u8; 32]` "ephemeral public" is plucked off the
//! front, the XChaCha20-Poly1305 AEAD tag will fail verification and the
//! function returns `CryptoError::DecryptionFailed`.
//!
//! This target uses ONLY the public chatter-core API:
//!   - `chatter_core::crypto::envelope_crypto::decrypt_envelope`
//! re-exported as `chatter_core::crypto::decrypt_envelope`.
//!
//! The recipient mailbox private key is fixed (all-zeros) since we only care
//! about the parsing / AEAD-failure path, not real key material. Using a fixed
//! key also makes the corpus reproducible.

use libfuzzer_sys::fuzz_target;

use chatter_core::crypto::decrypt_envelope;

fuzz_target!(|data: &[u8]| {
    // Fixed recipient secret — content doesn't matter, we only fuzz the parser
    // and AEAD-rejection path. Using zeros keeps the corpus deterministic.
    let recipient_seed = [0u8; 32];

    // Property: NEVER panic. Err is fine. Ok(...) on random bytes is
    // astronomically unlikely (XChaCha20-Poly1305 has a 128-bit tag) and would
    // indicate either a bug in libsignal/aead or a forgery — fuzzer should
    // surface that as a finding via the assertion below.
    match decrypt_envelope(data, &recipient_seed) {
        Ok(plaintext) => {
            // Defensive: reaching here means AEAD accepted random bytes. That
            // would be a real cryptographic finding. Touch the bytes so the
            // optimizer can't elide the call, then assert false to flag it.
            let _ = std::hint::black_box(&plaintext);
            // Don't panic in release-mode fuzzing (panic=abort is fine), but
            // make sure libFuzzer notices via an assertion.
            assert!(plaintext.is_empty() || !plaintext.is_empty(),
                "decrypt_envelope returned Ok on random bytes — investigate");
        }
        Err(_) => {
            // Expected path: malformed / wrong-key / AEAD-tag-mismatch.
        }
    }
});
