//! Mailbox envelope encryption for the NDSS '21 ephemeral-mailbox system.
//!
//! The envelope is libsignal's RFC 9180 HPKE (`signal_crypto::SimpleHpkeSender`
//! / `SimpleHpkeReceiver`, base mode, DHKEM(X25519, HKDF-SHA256), HKDF-SHA256,
//! AES-256-GCM) sealed to the recipient mailbox's ENCRYPTION key. Wire format,
//! exactly as libsignal emits it (rust/crypto/src/hpke.rs):
//!
//! ```text
//! type (1, = 1) || enc (32, ephemeral X25519 public) || ciphertext || tag (16)
//! ```
//!
//! This is STATELESS - no Signal session store required.
//!
//! # Two keys per mailbox
//!
//! A mailbox has an Ed25519 SIGNING key (its address: the server verifies the
//! blind-signed creation and every poll/ack challenge against it) and a
//! separate X25519 ENCRYPTION key generated alongside it
//! ([`generate_mailbox_encryption_key`]). Envelopes are sealed to the
//! encryption key only, so no key does both signing and DH, and the signing
//! key cannot open an envelope. The server never sees the encryption key: it
//! rides inside the end-to-end bootstrap announcement
//! (`mailbox::dm::bootstrap::MailboxAnnouncement`).
//!
//! # Why the prefix is ephemeral
//!
//! The envelope is deposited on the UNAUTHENTICATED ephemeral-mailbox
//! endpoint, so its prefix is readable by the server on every deposit. HPKE's
//! `enc` is a fresh X25519 public key per seal, so the prefix names nobody
//! (an earlier hand-rolled revision once put the sender's long-term identity
//! key there).
//!
//! This layer provides NO sender authentication (HPKE base mode is
//! unauthenticated): anyone can deposit a well-formed envelope in a mailbox
//! whose encryption key they know. Sender authentication lives in the inner
//! Signal layer (sealed sender certificate + Double Ratchet).
//!
//! # Binding
//!
//! DHKEM's key schedule already binds `enc || pkR` (RFC 9180 §4.1
//! `kem_context`). The recipient's X25519 key is additionally passed as the
//! AEAD AAD, and [`ENVELOPE_HPKE_INFO`] is the HPKE `info`, which separates
//! this use from any other HPKE use.

use libsignal_protocol::{KeyPair, PrivateKey, PublicKey};
use signal_crypto::{SimpleHpkeReceiver, SimpleHpkeSender};

use crate::error::CryptoError;

/// HPKE `info` for the mailbox envelope. The version lives nowhere else on
/// the wire, so changing it is a wire-format break (v2: sealed to the
/// mailbox's own X25519 encryption key, not a converted signing key).
const ENVELOPE_HPKE_INFO: &[u8] = b"NDSS21-MailboxEnvelope-HPKE-v2";

/// libsignal HPKE overhead: type byte + X25519 `enc` + AES-GCM tag.
const ENVELOPE_OVERHEAD: usize = 1 + 32 + 16;

fn decrypt_failure(err: CryptoError) -> CryptoError {
    tracing::warn!(
        target: "chatter::crypto::decrypt_failure",
        kind = "sealed_sender",
        error = %err,
        "decryption failure"
    );
    err
}

/// A fresh X25519 mailbox encryption key pair: `(private, public)`, each the
/// raw 32 bytes. Generated per mailbox, next to its Ed25519 signing key.
pub fn generate_mailbox_encryption_key() -> ([u8; 32], [u8; 32]) {
    use rand::TryRngCore;
    let pair = KeyPair::generate(&mut rand::rngs::OsRng.unwrap_err());
    let mut private = [0u8; 32];
    private.copy_from_slice(&pair.private_key.serialize());
    let mut public = [0u8; 32];
    public.copy_from_slice(pair.public_key.public_key_bytes());
    (private, public)
}

/// The public half of a mailbox encryption key.
pub fn mailbox_encryption_public_key(private: &[u8; 32]) -> Result<[u8; 32], CryptoError> {
    let public = PrivateKey::deserialize(private)
        .and_then(|sk| sk.public_key())
        .map_err(|e| CryptoError::InvalidKey(e.to_string()))?;
    let mut out = [0u8; 32];
    out.copy_from_slice(public.public_key_bytes());
    Ok(out)
}

/// Encrypt an envelope for a mailbox recipient.
///
/// HPKE generates a fresh ephemeral key per call; nothing about the sender is
/// carried at this layer — see the module docs.
///
/// * `recipient_encryption_public` - the recipient mailbox's X25519
///   ENCRYPTION key (never its Ed25519 address/signing key).
///
/// Returns `type (1) || enc (32) || ciphertext || tag (16)`.
pub fn encrypt_envelope(
    plaintext: &[u8],
    recipient_encryption_public: &[u8; 32],
) -> Result<Vec<u8>, CryptoError> {
    let recipient_pk = PublicKey::from_djb_public_key_bytes(recipient_encryption_public)
        .map_err(|e| CryptoError::InvalidKey(e.to_string()))?;

    // A low-order recipient key yields an all-zero DH output, which libsignal's
    // `calculate_agreement` rejects (rust/core/src/curve.rs), so seal fails.
    recipient_pk
        .seal(ENVELOPE_HPKE_INFO, recipient_encryption_public, plaintext)
        .map_err(|e| CryptoError::EncryptionFailed(format!("mailbox envelope HPKE seal: {e}")))
}

/// Decrypt an envelope received at a mailbox.
///
/// * `recipient_encryption_private` - our mailbox's X25519 ENCRYPTION key.
///
/// There is deliberately no sender output: the prefix is a per-message
/// ephemeral key that identifies nobody, and sender authentication is the
/// inner Signal layer's job (see module docs).
pub fn decrypt_envelope(
    envelope: &[u8],
    recipient_encryption_private: &[u8; 32],
) -> Result<Vec<u8>, CryptoError> {
    if envelope.len() < ENVELOPE_OVERHEAD {
        return Err(decrypt_failure(CryptoError::DecryptionFailed(
            "Envelope too short".into(),
        )));
    }

    let recipient_sk = PrivateKey::deserialize(recipient_encryption_private)
        .map_err(|e| CryptoError::InvalidKey(e.to_string()))?;
    let recipient_public = mailbox_encryption_public_key(recipient_encryption_private)?;

    // A low-order `enc` yields an all-zero DH output, which libsignal rejects.
    recipient_sk
        .open(ENVELOPE_HPKE_INFO, &recipient_public, envelope)
        .map_err(|e| {
            decrypt_failure(CryptoError::DecryptionFailed(format!(
                "mailbox envelope HPKE open: {e}"
            )))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encryption_public_key_matches_generated_pair() {
        let (private, public) = generate_mailbox_encryption_key();
        assert_eq!(mailbox_encryption_public_key(&private).unwrap(), public);
    }

    #[test]
    fn test_envelope_roundtrip() {
        let (private, public) = generate_mailbox_encryption_key();
        let plaintext = b"Hello, this is a secret message!";
        let envelope = encrypt_envelope(plaintext, &public).unwrap();
        assert_eq!(envelope.len(), plaintext.len() + ENVELOPE_OVERHEAD);
        assert_eq!(
            envelope[0], 1,
            "libsignal HPKE type byte (Base_X25519_HkdfSha256_Aes256Gcm)"
        );
        assert_eq!(decrypt_envelope(&envelope, &private).unwrap(), plaintext);
    }

    /// The mailbox's Ed25519 signing key is not a decryption key: an envelope
    /// sealed to the mailbox's encryption key does not open under the signing
    /// seed, nor under the signing key's clamped SHA-512 scalar (the old
    /// Ed25519->X25519 conversion), and sealing to the signing PUBLIC key
    /// produces nothing the encryption key opens.
    #[test]
    fn test_signing_key_cannot_open_envelope() {
        use ed25519_dalek::SigningKey;
        use sha2::{Digest, Sha512};

        let mut seed = [0u8; 32];
        {
            use rand::TryRngCore;
            rand::rngs::OsRng
                .try_fill_bytes(&mut seed)
                .expect("OS CSPRNG must not fail");
        }
        let signing = SigningKey::from_bytes(&seed);
        let (enc_private, enc_public) = generate_mailbox_encryption_key();
        assert_ne!(enc_public, signing.verifying_key().to_bytes());

        let envelope = encrypt_envelope(b"bootstrap", &enc_public).unwrap();
        assert!(
            decrypt_envelope(&envelope, &seed).is_err(),
            "signing seed opened the envelope"
        );
        let mut scalar = [0u8; 32];
        scalar.copy_from_slice(&Sha512::digest(seed)[..32]);
        assert!(
            decrypt_envelope(&envelope, &scalar).is_err(),
            "converted signing scalar opened the envelope"
        );
        assert_eq!(
            decrypt_envelope(&envelope, &enc_private).unwrap(),
            b"bootstrap"
        );
    }

    #[test]
    fn test_envelope_empty_plaintext_roundtrip() {
        let (private, public) = generate_mailbox_encryption_key();
        let envelope = encrypt_envelope(b"", &public).unwrap();
        assert_eq!(decrypt_envelope(&envelope, &private).unwrap(), b"");
    }

    #[test]
    fn test_envelope_wrong_recipient() {
        let (_, public) = generate_mailbox_encryption_key();
        let (wrong_private, _) = generate_mailbox_encryption_key();
        let envelope = encrypt_envelope(b"Secret message", &public).unwrap();
        assert!(decrypt_envelope(&envelope, &wrong_private).is_err());
    }

    /// Every byte is authenticated: type, `enc`, ciphertext and tag.
    #[test]
    fn test_envelope_tamper_anywhere_rejected() {
        let (private, public) = generate_mailbox_encryption_key();
        let envelope = encrypt_envelope(b"Verified message", &public).unwrap();
        for i in [0, 1, 32, 33, envelope.len() - 17, envelope.len() - 1] {
            let mut t = envelope.clone();
            t[i] ^= 0x01;
            assert!(
                decrypt_envelope(&t, &private).is_err(),
                "flip at byte {i} accepted"
            );
        }
        assert!(decrypt_envelope(&envelope[..envelope.len() - 1], &private).is_err());
        assert!(decrypt_envelope(&envelope[..ENVELOPE_OVERHEAD - 1], &private).is_err());
    }

    /// The `info` label and the recipient-key AAD are both load-bearing: an
    /// HPKE seal to the same key under a different info or AAD must not open.
    #[test]
    fn test_envelope_info_and_aad_are_bound() {
        let (private, public) = generate_mailbox_encryption_key();
        let pk = PublicKey::from_djb_public_key_bytes(&public).unwrap();

        // Positive control: same construction as encrypt_envelope.
        let ok = pk.seal(ENVELOPE_HPKE_INFO, &public, b"m").unwrap();
        assert_eq!(decrypt_envelope(&ok, &private).unwrap(), b"m");

        let wrong_info = pk
            .seal(b"NDSS21-MailboxEnvelope-HPKE-v1", &public, b"m")
            .unwrap();
        assert!(decrypt_envelope(&wrong_info, &private).is_err());
        let no_aad = pk.seal(ENVELOPE_HPKE_INFO, &[], b"m").unwrap();
        assert!(decrypt_envelope(&no_aad, &private).is_err());
    }

    /// An all-zero (low-order) `enc` must be refused, not open under a
    /// predictable key.
    #[test]
    fn test_envelope_low_order_enc_rejected() {
        let (private, public) = generate_mailbox_encryption_key();
        let mut envelope = encrypt_envelope(b"m", &public).unwrap();
        envelope[1..33].fill(0);
        assert!(decrypt_envelope(&envelope, &private).is_err());
    }

    /// The deposit is anonymous: the prefix must be fresh per message.
    #[test]
    fn test_envelope_prefix_is_ephemeral() {
        let (private, public) = generate_mailbox_encryption_key();
        let first = encrypt_envelope(b"bootstrap", &public).unwrap();
        let second = encrypt_envelope(b"bootstrap", &public).unwrap();
        assert_ne!(first[1..33], second[1..33], "enc must be fresh per message");
        assert_eq!(decrypt_envelope(&first, &private).unwrap(), b"bootstrap");
    }

    /// The HPKE info IS the version. Changing it is a wire-format break.
    #[test]
    fn envelope_version_label_is_pinned() {
        assert_eq!(ENVELOPE_HPKE_INFO, b"NDSS21-MailboxEnvelope-HPKE-v2");
    }
}
