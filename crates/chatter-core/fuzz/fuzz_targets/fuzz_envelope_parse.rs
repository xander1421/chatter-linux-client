#![no_main]
//! Fuzz target: envelope decryption / parsing path.
//!
//! Property: `decrypt_envelope` must NEVER panic on arbitrary input.
//! Returning `Err` is the expected outcome for random bytes; even if the
//! length check passes and an HPKE `enc` is plucked off the front, the
//! AES-256-GCM tag will fail verification and the function returns
//! `CryptoError::DecryptionFailed`.
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

    // Both outcomes are valid corpus inputs: the fixed seed also lets callers
    // construct a valid encrypted envelope for the success path.
    let _ = std::hint::black_box(decrypt_envelope(data, &recipient_seed));
});
