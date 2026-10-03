//! HPKE to a Signal identity key, for the two messages sent before any
//! session exists: the NDSS'21 anonymous-DM initial contact (Phase 1) and the
//! friend-request intro note.
//!
//! Unlike X3DH this needs no prekey fetch: the sender already holds the
//! recipient's long-term identity key (from a friend code or a bundle).
//! Signal identity keys are Curve25519 keys used both for XEdDSA signatures
//! and for X25519 agreement (as in X3DH), so sealing to one is the normal
//! Signal use of that key, with no Ed25519 conversion involved.
//!
//! # Construction
//!
//! libsignal's RFC 9180 HPKE (`signal_crypto::SimpleHpkeSender` /
//! `SimpleHpkeReceiver`): base mode, DHKEM(X25519, HKDF-SHA256),
//! HKDF-SHA256, AES-256-GCM — the same implementation as the mailbox
//! envelope (`envelope_crypto`). Each use has its own HPKE `info`
//! ([`IdentityHpkeUse`]), and the recipient's serialized identity key is the
//! AEAD AAD. Wire format, as libsignal emits it:
//!
//! ```text
//! type (1, = 1) || enc (32, ephemeral X25519 public) || ciphertext || tag (16)
//! ```
//!
//! # Where the secret lives
//!
//! Opening needs the identity private key, so [`open_with_identity`] takes
//! a typed `IdentityKeyPair` and is called only inside the Signal actor
//! (`SignalActor::open_identity_hpke`); no raw identity secret bytes leave
//! it for this.
//!
//! # Security
//!
//! - **Sender authentication**: none at this layer (base mode).
//! - **No forward secrecy**: a later compromise of the recipient's identity
//!   key opens past messages, so these carry only what Phase 2 needs
//!   (ephemeral mailbox key, identity key, delivery token, short note) —
//!   never sensitive content. Full Double Ratchet sessions follow.

use libsignal_protocol::{IdentityKey, IdentityKeyPair};
use signal_crypto::{SimpleHpkeReceiver, SimpleHpkeSender};

use crate::error::CryptoError;

/// libsignal HPKE overhead: type byte + X25519 `enc` + AES-GCM tag.
pub const OVERHEAD: usize = 1 + 32 + 16;

/// What an identity-key HPKE message is for. Each use is a distinct HPKE
/// `info`, so a ciphertext made for one never opens as the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityHpkeUse {
    /// NDSS'21 anonymous-DM initial contact (`AnonDmInitMessage`).
    AnonDmInit,
    /// Friend-request intro note.
    FriendRequest,
}

impl IdentityHpkeUse {
    /// The HPKE `info`; changing one is a wire-format break.
    pub const fn info(self) -> &'static [u8] {
        match self {
            Self::AnonDmInit => b"Chatter-AnonDM-InitContact-HPKE-v2",
            Self::FriendRequest => b"Chatter-FriendRequest-Note-HPKE-v1",
        }
    }
}

/// Seal `plaintext` to `recipient`'s identity key.
pub fn seal_to_identity(
    recipient: &IdentityKey,
    purpose: IdentityHpkeUse,
    plaintext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    // A low-order recipient key yields an all-zero DH output, which libsignal
    // rejects, so seal fails.
    recipient
        .public_key()
        .seal(purpose.info(), &recipient.serialize(), plaintext)
        .map_err(|e| CryptoError::EncryptionFailed(format!("identity HPKE seal: {e}")))
}

/// Open a message sealed to `ours`. Called inside the Signal actor.
pub fn open_with_identity(
    ours: &IdentityKeyPair,
    purpose: IdentityHpkeUse,
    ciphertext: &[u8],
) -> Result<Vec<u8>, CryptoError> {
    if ciphertext.len() < OVERHEAD {
        return Err(CryptoError::DecryptionFailed("identity HPKE message too short".into()));
    }
    ours.private_key()
        .open(purpose.info(), &ours.identity_key().serialize(), ciphertext)
        .map_err(|e| CryptoError::DecryptionFailed(format!("identity HPKE open: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::TryRngCore;

    fn identity() -> IdentityKeyPair {
        IdentityKeyPair::generate(&mut rand::rngs::OsRng.unwrap_err())
    }

    #[test]
    fn roundtrip() {
        let ours = identity();
        let plaintext = b"Hello, this is an initial contact message!";
        let sealed = seal_to_identity(ours.identity_key(), IdentityHpkeUse::AnonDmInit, plaintext).unwrap();
        assert_eq!(sealed.len(), plaintext.len() + OVERHEAD);
        assert_eq!(sealed[0], 1, "libsignal HPKE type byte");
        let opened = open_with_identity(&ours, IdentityHpkeUse::AnonDmInit, &sealed).unwrap();
        assert_eq!(opened, plaintext);
    }

    #[test]
    fn empty_and_large_plaintext() {
        let ours = identity();
        for plaintext in [Vec::new(), (0..1_000_000).map(|i| (i % 256) as u8).collect()] {
            let sealed = seal_to_identity(ours.identity_key(), IdentityHpkeUse::FriendRequest, &plaintext).unwrap();
            assert_eq!(open_with_identity(&ours, IdentityHpkeUse::FriendRequest, &sealed).unwrap(), plaintext);
        }
    }

    #[test]
    fn wrong_identity_fails() {
        let sealed = seal_to_identity(identity().identity_key(), IdentityHpkeUse::AnonDmInit, b"secret").unwrap();
        assert!(open_with_identity(&identity(), IdentityHpkeUse::AnonDmInit, &sealed).is_err());
    }

    /// The two uses are domain-separated.
    #[test]
    fn uses_do_not_cross() {
        let ours = identity();
        let sealed = seal_to_identity(ours.identity_key(), IdentityHpkeUse::FriendRequest, b"note").unwrap();
        assert!(open_with_identity(&ours, IdentityHpkeUse::AnonDmInit, &sealed).is_err());
    }

    #[test]
    fn tamper_anywhere_rejected() {
        let ours = identity();
        let sealed = seal_to_identity(ours.identity_key(), IdentityHpkeUse::AnonDmInit, b"important").unwrap();
        for i in [0, 1, 32, 33, sealed.len() - 1] {
            let mut t = sealed.clone();
            t[i] ^= 0x01;
            assert!(open_with_identity(&ours, IdentityHpkeUse::AnonDmInit, &t).is_err(), "flip at {i}");
        }
        assert!(open_with_identity(&ours, IdentityHpkeUse::AnonDmInit, &sealed[..OVERHEAD - 1]).is_err());
    }

    #[test]
    fn encryption_is_randomized() {
        let ours = identity();
        let a = seal_to_identity(ours.identity_key(), IdentityHpkeUse::AnonDmInit, b"same").unwrap();
        let b = seal_to_identity(ours.identity_key(), IdentityHpkeUse::AnonDmInit, b"same").unwrap();
        assert_ne!(a, b);
    }
}
