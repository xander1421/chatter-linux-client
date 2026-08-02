//! Sealed Sender envelope encryption for NDSS '21 mailbox system.
//!
//! Implements the transport layer encryption for ephemeral mailboxes.
//! The envelope is encrypted using ECDH between a FRESH per-message ephemeral
//! keypair and the recipient's mailbox key.
//!
//! This is STATELESS - no Signal session store required.
//!
//! # Protocol (NDSS '21)
//!
//! - Sender encrypts: `ECDH(fresh_ephemeral_private, recipient_mailbox_public)`
//! - Recipient decrypts: `ECDH(recipient_mailbox_private, ephemeral_public)`
//!
//! # Why the prefix is ephemeral
//!
//! The envelope is deposited on the UNAUTHENTICATED ephemeral-mailbox
//! endpoint, so its first 32 bytes are readable by the server on every
//! deposit. An earlier revision put the sender's long-term Signal identity
//! public key there — the exact bytes the server publishes as that user's
//! `identity_key` — which named the sender of the one flow whose entire
//! purpose is sender anonymity. The prefix is now a fresh X25519 public key
//! generated per envelope, matching sealed sender v2 (libsignal
//! `rust/protocol/src/sealed_sender.rs`, `UnidentifiedSenderMessage::V2`).
//!
//! This layer therefore provides NO sender authentication and never did:
//! anyone can deposit a well-formed envelope in a mailbox whose public key
//! they know. Sender authentication lives in the inner Signal layer (sealed
//! sender certificate + Double Ratchet), which is where it is actually
//! checked.
//!
//! # Key Conversion
//!
//! Mailbox keys are Ed25519 (for signing). For ECDH, we convert to X25519
//! using the birational map defined in the XEdDSA specification:
//!
//! ```text
//! u = (1 + y) / (1 - y) mod p
//! ```
//!
//! Where y is the Ed25519 point and u is the X25519 point.

use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use curve25519_dalek::edwards::CompressedEdwardsY;
use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret as X25519PrivateKey};
use zeroize::Zeroize;

use crate::error::CryptoError;

/// Nonce size for XChaCha20-Poly1305 (24 bytes).
const NONCE_SIZE: usize = 24;

/// HKDF info label for envelope key derivation. The derived key is bound to
/// both endpoints by appending `ephemeral_x25519_pub || recipient_x25519_pub`
/// after this label per RFC 5869 §3.3 — this prevents cross-mailbox key
/// reuse when a sender talks to multiple anonymous mailboxes that share an
/// X25519 keypair, and gives explicit destination binding the inner Signal
/// layer was previously relied on to provide.
///
/// v3 = ephemeral sender prefix. v2 envelopes (long-term identity prefix) do
/// not decrypt under this label and are not supported.
const ENVELOPE_HKDF_INFO: &[u8] = b"NDSS21-MailboxEnvelope-v3";

/// Convert an Ed25519 public key to X25519 public key.
///
/// Uses the birational map from the XEdDSA specification:
/// u = (1 + y) / (1 - y) mod p
///
/// # Arguments
///
/// * `ed25519_pk` - 32-byte Ed25519 public key
///
/// # Returns
///
/// 32-byte X25519 public key, or error if conversion fails.
pub fn ed25519_to_x25519_public(ed25519_pk: &[u8; 32]) -> Result<[u8; 32], CryptoError> {
    // Decompress the Ed25519 point
    let compressed = CompressedEdwardsY(*ed25519_pk);
    let point = compressed
        .decompress()
        .ok_or_else(|| CryptoError::InvalidKey("Invalid Ed25519 public key".into()))?;

    // Convert to Montgomery form (X25519)
    let montgomery = point.to_montgomery();
    Ok(montgomery.to_bytes())
}

/// Convert an Ed25519 private key (seed) to X25519 private key.
///
/// The Ed25519 seed is hashed with SHA-512, and the first 32 bytes
/// (after clamping) become the X25519 private key.
///
/// # Arguments
///
/// * `ed25519_seed` - 32-byte Ed25519 private key seed
///
/// # Returns
///
/// X25519 private key.
pub fn ed25519_to_x25519_private(ed25519_seed: &[u8; 32]) -> X25519PrivateKey {
    // Hash the seed with SHA-512 (same as Ed25519 key derivation)
    use sha2::{Digest, Sha512};
    let hash = Sha512::digest(ed25519_seed);

    // Take first 32 bytes and clamp (X25519 clamping is done by StaticSecret)
    let mut scalar = [0u8; 32];
    scalar.copy_from_slice(&hash[..32]);

    X25519PrivateKey::from(scalar)
}

/// Encrypt an envelope for a mailbox recipient.
///
/// A fresh X25519 keypair is generated for every call; its public half is the
/// envelope prefix. Nothing about the sender is carried at this layer — see
/// the module docs.
///
/// # Arguments
///
/// * `plaintext` - The content to encrypt (Signal ciphertext)
/// * `recipient_mailbox_public` - Recipient's mailbox public key (Ed25519, will be converted)
///
/// # Returns
///
/// Encrypted envelope: `ephemeral_public (32) || nonce (24) || ciphertext || tag (16)`
pub fn encrypt_envelope(
    plaintext: &[u8],
    recipient_mailbox_public: &[u8; 32],
) -> Result<Vec<u8>, CryptoError> {
    // Convert recipient's Ed25519 mailbox key to X25519
    let recipient_x25519 = ed25519_to_x25519_public(recipient_mailbox_public)?;
    let recipient_pk = X25519PublicKey::from(recipient_x25519);

    // Fresh per-envelope sender key. Drawn straight from the OS CSPRNG rather
    // than a userspace PRG so a memory disclosure cannot reproduce the series.
    let mut ephemeral_seed = [0u8; 32];
    {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut ephemeral_seed).expect("OS CSPRNG must not fail");}
    let ephemeral_private = X25519PrivateKey::from(ephemeral_seed);
    ephemeral_seed.zeroize();
    let ephemeral_public = X25519PublicKey::from(&ephemeral_private);

    // Compute ECDH shared secret
    let shared_secret = ephemeral_private.diffie_hellman(&recipient_pk);

    // RFC 7748 §6.1: reject low-order public keys that collapse ECDH to a
    // constant. Without this gate an attacker can submit a low-order
    // mailbox-public substitute and force every recipient to derive the
    // same envelope key.
    if !shared_secret.was_contributory() {
        return Err(CryptoError::KeyDerivationFailed(
            "non-contributory ECDH (low-order public key)".into(),
        ));
    }

    // Derive encryption key using HKDF, binding both endpoint public keys
    // into the info parameter (see ENVELOPE_HKDF_INFO doc comment).
    let hkdf = Hkdf::<Sha256>::new(None, shared_secret.as_bytes());
    let mut info = Vec::with_capacity(ENVELOPE_HKDF_INFO.len() + 64);
    info.extend_from_slice(ENVELOPE_HKDF_INFO);
    info.extend_from_slice(ephemeral_public.as_bytes());
    info.extend_from_slice(recipient_pk.as_bytes());
    let mut encryption_key = [0u8; 32];
    hkdf.expand(&info, &mut encryption_key)
        .map_err(|_| CryptoError::KeyDerivationFailed("HKDF expand failed".into()))?;

    // Generate random nonce
    let mut nonce_bytes = [0u8; NONCE_SIZE];
    {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut nonce_bytes).expect("OS CSPRNG must not fail");}
    let nonce = XNonce::from_slice(&nonce_bytes);

    // Encrypt with XChaCha20-Poly1305
    let cipher = XChaCha20Poly1305::new_from_slice(&encryption_key)
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    // Zeroize encryption key after creating cipher (key is copied into cipher)
    encryption_key.zeroize();

    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| CryptoError::EncryptionFailed(e.to_string()))?;

    // Build envelope: ephemeral_public (32) || nonce (24) || ciphertext
    let mut envelope = Vec::with_capacity(32 + NONCE_SIZE + ciphertext.len());
    envelope.extend_from_slice(ephemeral_public.as_bytes());
    envelope.extend_from_slice(&nonce_bytes);
    envelope.extend_from_slice(&ciphertext);

    Ok(envelope)
}

/// Decrypt an envelope received at a mailbox.
///
/// # Arguments
///
/// * `envelope` - The encrypted envelope
/// * `recipient_mailbox_private` - Recipient's mailbox private key (Ed25519 seed, will be converted)
///
/// # Returns
///
/// The decrypted plaintext. There is deliberately no sender output: the
/// prefix is a per-message ephemeral key that identifies nobody, and sender
/// authentication is the inner Signal layer's job (see module docs).
pub fn decrypt_envelope(
    envelope: &[u8],
    recipient_mailbox_private: &[u8; 32],
) -> Result<Vec<u8>, CryptoError> {
    // Minimum size: ephemeral_pk (32) + nonce (24) + tag (16)
    if envelope.len() < 32 + NONCE_SIZE + 16 {
        let err = CryptoError::DecryptionFailed("Envelope too short".into());
        tracing::warn!(
            target: "chatter::crypto::decrypt_failure",
            kind = "sealed_sender",
            error = %err,
            "decryption failure"
        );
        return Err(err);
    }

    // Extract the sender's per-message ephemeral public key
    let mut ephemeral_public = [0u8; 32];
    ephemeral_public.copy_from_slice(&envelope[..32]);

    // Extract nonce and ciphertext
    let nonce = XNonce::from_slice(&envelope[32..32 + NONCE_SIZE]);
    let ciphertext = &envelope[32 + NONCE_SIZE..];

    // Convert recipient's Ed25519 private key to X25519
    let recipient_x25519 = ed25519_to_x25519_private(recipient_mailbox_private);
    let recipient_x25519_public = X25519PublicKey::from(&recipient_x25519);

    // Compute ECDH shared secret
    let ephemeral_pk = X25519PublicKey::from(ephemeral_public);
    let shared_secret = recipient_x25519.diffie_hellman(&ephemeral_pk);

    // RFC 7748 §6.1: reject low-order sender public keys that collapse
    // ECDH to a constant. Without this gate an attacker can blast envelopes
    // with a low-order sender key and force every recipient to derive a
    // deterministic decryption key.
    if !shared_secret.was_contributory() {
        let err = CryptoError::DecryptionFailed(
            "non-contributory ECDH (low-order sender key)".into(),
        );
        tracing::warn!(
            target: "chatter::crypto::decrypt_failure",
            kind = "sealed_sender",
            error = %err,
            "decryption failure"
        );
        return Err(err);
    }

    // Derive decryption key using HKDF, binding both endpoint public keys
    // into the info parameter (see ENVELOPE_HKDF_INFO doc comment).
    let hkdf = Hkdf::<Sha256>::new(None, shared_secret.as_bytes());
    let mut info = Vec::with_capacity(ENVELOPE_HKDF_INFO.len() + 64);
    info.extend_from_slice(ENVELOPE_HKDF_INFO);
    info.extend_from_slice(&ephemeral_public);
    info.extend_from_slice(recipient_x25519_public.as_bytes());
    let mut decryption_key = [0u8; 32];
    hkdf.expand(&info, &mut decryption_key)
        .map_err(|_| {
            let err = CryptoError::KeyDerivationFailed("HKDF expand failed".into());
            tracing::warn!(
                target: "chatter::crypto::decrypt_failure",
                kind = "sealed_sender",
                error = %err,
                "decryption failure"
            );
            err
        })?;

    // Decrypt with XChaCha20-Poly1305
    let cipher = XChaCha20Poly1305::new_from_slice(&decryption_key)
        .map_err(|e| {
            let err = CryptoError::DecryptionFailed(e.to_string());
            tracing::warn!(
                target: "chatter::crypto::decrypt_failure",
                kind = "sealed_sender",
                error = %err,
                "decryption failure"
            );
            err
        })?;

    // Zeroize decryption key after creating cipher (key is copied into cipher)
    decryption_key.zeroize();

    let plaintext = cipher
        .decrypt(nonce, ciphertext)
        .map_err(|e| {
            let err = CryptoError::DecryptionFailed(e.to_string());
            tracing::warn!(
                target: "chatter::crypto::decrypt_failure",
                kind = "sealed_sender",
                error = %err,
                "decryption failure"
            );
            err
        })?;

    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[test]
    fn test_ed25519_to_x25519_conversion() {
        // Generate Ed25519 keypair
        let mut seed = [0u8; 32];
        {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut seed).expect("OS CSPRNG must not fail");}
        let signing_key = SigningKey::from_bytes(&seed);
        let verifying_key = signing_key.verifying_key();

        // Convert public key
        let x25519_public = ed25519_to_x25519_public(verifying_key.as_bytes()).unwrap();
        assert_eq!(x25519_public.len(), 32);

        // Convert private key
        let x25519_private = ed25519_to_x25519_private(&seed);

        // Verify the public key matches what we'd get from the private key
        let derived_public = X25519PublicKey::from(&x25519_private);
        assert_eq!(x25519_public, *derived_public.as_bytes());
    }

    #[test]
    fn test_envelope_roundtrip() {
        // Generate recipient mailbox (Ed25519)
        let mut recipient_seed = [0u8; 32];
        {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut recipient_seed).expect("OS CSPRNG must not fail");}
        let recipient_signing = SigningKey::from_bytes(&recipient_seed);
        let recipient_public = recipient_signing.verifying_key();

        // Encrypt
        let plaintext = b"Hello, this is a secret message!";
        let envelope = encrypt_envelope(plaintext, recipient_public.as_bytes()).unwrap();

        // Decrypt
        let decrypted = decrypt_envelope(&envelope, &recipient_seed).unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_envelope_wrong_recipient() {
        // Generate recipient mailbox
        let mut recipient_seed = [0u8; 32];
        {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut recipient_seed).expect("OS CSPRNG must not fail");}
        let recipient_signing = SigningKey::from_bytes(&recipient_seed);
        let recipient_public = recipient_signing.verifying_key();

        // Generate wrong recipient
        let mut wrong_seed = [0u8; 32];
        {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut wrong_seed).expect("OS CSPRNG must not fail");}

        // Encrypt to correct recipient
        let envelope = encrypt_envelope(b"Secret message", recipient_public.as_bytes()).unwrap();

        // Try to decrypt with wrong recipient - should fail
        let result = decrypt_envelope(&envelope, &wrong_seed);
        assert!(result.is_err());
    }

    /// The whole point of the ephemeral-mailbox deposit is that the server
    /// cannot tell who sent it. The 32-byte envelope prefix is public to the
    /// server, so it must be a per-message ephemeral key — never the sender's
    /// long-term Signal identity public key, which the server itself publishes
    /// under `identity_key` and can index by.
    #[test]
    fn test_envelope_prefix_is_ephemeral_not_sender_identity() {
        // The sender's long-term identity, as the server publishes it.
        let mut identity_seed = [0u8; 32];
        {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut identity_seed).expect("OS CSPRNG must not fail");}
        let sender_identity = X25519PrivateKey::from(identity_seed);
        let published_identity = *X25519PublicKey::from(&sender_identity).as_bytes();

        // Recipient mailbox (Ed25519).
        let mut recipient_seed = [0u8; 32];
        {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut recipient_seed).expect("OS CSPRNG must not fail");}
        let recipient_public = SigningKey::from_bytes(&recipient_seed).verifying_key();

        let first = encrypt_envelope(b"bootstrap", recipient_public.as_bytes()).unwrap();
        let second = encrypt_envelope(b"bootstrap", recipient_public.as_bytes()).unwrap();

        assert_ne!(
            first[..32],
            published_identity[..],
            "envelope prefix leaks the sender's published identity key"
        );
        assert_ne!(
            first[..32],
            second[..32],
            "envelope prefix must be fresh per message, not a reusable linker"
        );

        // Still decryptable by the mailbox owner.
        let decrypted = decrypt_envelope(&first, &recipient_seed).unwrap();
        assert_eq!(decrypted, b"bootstrap");
    }

    /// A tampered prefix must fail the AEAD, not silently decrypt: the prefix
    /// is bound into the HKDF info, so swapping it changes the key.
    #[test]
    fn test_envelope_tampered_prefix_rejected() {
        let mut recipient_seed = [0u8; 32];
        {use rand::TryRngCore; rand::rngs::OsRng.try_fill_bytes(&mut recipient_seed).expect("OS CSPRNG must not fail");}
        let recipient_public = SigningKey::from_bytes(&recipient_seed).verifying_key();

        let mut envelope = encrypt_envelope(b"Verified message", recipient_public.as_bytes()).unwrap();
        envelope[0] ^= 0x01;

        assert!(decrypt_envelope(&envelope, &recipient_seed).is_err());
    }
}
