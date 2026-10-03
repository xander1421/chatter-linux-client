//! Identity key operations.
//!
//! Identity key pair generation and management for Signal Protocol.

use libsignal_protocol::{IdentityKey, IdentityKeyPair};
use rand::{CryptoRng, Rng};
use std::sync::Arc;

use crate::error::CryptoError;
use crate::storage::KeyStore;

/// Identity key manager.
///
/// Handles generation, storage, and retrieval of the local device's
/// identity key pair used for Signal Protocol encryption.
#[derive(Clone)]
pub struct IdentityKeyManager {
    key_store: Arc<KeyStore>,
}

impl IdentityKeyManager {
    /// Create a new IdentityKeyManager with the given key store.
    pub fn new(key_store: Arc<KeyStore>) -> Self {
        Self { key_store }
    }

    /// Reset all local crypto state so a fresh registration can proceed.
    ///
    /// Only safe to call when there is no active user session — the caller
    /// MUST verify that first (see `AuthService::is_logged_in`). Used to
    /// recover from a registration attempt that persisted the identity
    /// locally but failed before the server confirmed the account.
    pub fn reset_for_new_registration(&self) -> Result<(), CryptoError> {
        self.key_store
            .clear_all_crypto_state()
            .map_err(CryptoError::Storage)
    }

    /// Generate a new identity key pair and registration ID.
    ///
    /// This should only be called once during device registration.
    /// The registration ID is a 14-bit value (1-16383) per Signal spec.
    ///
    /// Returns an error if an identity already exists (to prevent
    /// accidentally breaking all existing sessions).
    ///
    /// Returns the registration ID and identity key pair.
    pub fn generate_identity<R: CryptoRng + Rng>(
        &self,
        rng: &mut R,
    ) -> Result<(u32, IdentityKeyPair), CryptoError> {
        // Guard: never overwrite an existing identity — doing so would
        // silently break every established session.
        if self.has_identity()? {
            return Err(CryptoError::Protocol(
                "identity already exists — cannot overwrite".into(),
            ));
        }

        // Generate 14-bit registration ID (Signal spec: 1-16383, avoid 0)
        let registration_id = (rng.random::<u32>() % 0x3FFF) + 1;

        // Generate identity key pair
        let key_pair = IdentityKeyPair::generate(rng);

        // Store in database
        self.key_store
            .set_local_identity(registration_id, &key_pair.serialize())
            .map_err(CryptoError::Storage)?;

        Ok((registration_id, key_pair))
    }

    /// Get the local identity key pair and registration ID.
    ///
    /// Returns `None` if no identity has been generated yet.
    pub fn get_identity(&self) -> Result<Option<(u32, IdentityKeyPair)>, CryptoError> {
        let result = self
            .key_store
            .get_local_identity()
            .map_err(CryptoError::Storage)?;

        match result {
            None => Ok(None),
            Some((registration_id, key_pair_bytes)) => {
                let key_pair = IdentityKeyPair::try_from(key_pair_bytes.as_slice())
                    .map_err(|_| CryptoError::InvalidKeyFormat)?;
                Ok(Some((registration_id, key_pair)))
            }
        }
    }

    /// Check if a local identity exists.
    pub fn has_identity(&self) -> Result<bool, CryptoError> {
        self.key_store
            .has_local_identity()
            .map_err(CryptoError::Storage)
    }

    /// Get the local identity public key.
    ///
    /// Returns an error if no identity has been generated.
    pub fn get_public_key(&self) -> Result<IdentityKey, CryptoError> {
        let (_, key_pair) = self
            .get_identity()?
            .ok_or(CryptoError::NoIdentity)?;

        Ok(*key_pair.identity_key())
    }

    /// Get the local registration ID.
    ///
    /// Returns an error if no identity has been generated.
    pub fn get_registration_id(&self) -> Result<u32, CryptoError> {
        let (registration_id, _) = self
            .get_identity()?
            .ok_or(CryptoError::NoIdentity)?;

        Ok(registration_id)
    }

    /// Answer a server registration challenge with the identity key.
    ///
    /// The identity key also signs signed prekeys and Kyber prekeys over their
    /// raw serialized public keys, so signing an arbitrary server-chosen
    /// "nonce" would let a hostile server obtain a valid prekey signature over
    /// a key of its choosing (`0x05 || pub`, or a Kyber key) and strip PQXDH's
    /// post-quantum protection. There is deliberately no general-purpose
    /// identity `sign(data)`: the only thing signed here is
    /// [`registration_challenge_message`], which refuses any nonce that is not
    /// exactly [`REGISTRATION_CHALLENGE_NONCE_LEN`] bytes and domain-separates
    /// the rest.
    pub fn sign_registration_challenge(&self, nonce: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let message = registration_challenge_message(nonce)?;
        let (_, key_pair) = self
            .get_identity()?
            .ok_or(CryptoError::NoIdentity)?;

        // SECURITY: XEdDSA signing nonce must come from the OS, not a
        // userspace ChaCha PRG seeded once via ThreadRng. Memory disclosure
        // of the PRG state would reveal the nonce and let an attacker
        // recover the identity private key from the signature.
        use rand::TryRngCore;
        let mut rng = rand::rngs::OsRng.unwrap_err();
        key_pair.private_key().calculate_signature(&message, &mut rng)
            .map(|sig| sig.into_vec())
            .map_err(|e| CryptoError::Protocol(e.to_string()))
    }
}

/// Length of the nonce the server issues (`RegistrationChallengeNonceSize`,
/// internal/services/auth/service/registration_challenge.go).
pub const REGISTRATION_CHALLENGE_NONCE_LEN: usize = 32;

/// Domain prefix of the registration-challenge signature; must match the
/// server's `registrationChallengeDomain`.
const REGISTRATION_CHALLENGE_DOMAIN: &[u8] = b"chatter-reg-challenge-v1\0";

/// The exact bytes the identity key signs for a registration challenge:
/// domain prefix || nonce. Rejects a nonce of any other length — a 33-byte
/// serialized Curve25519 key or a Kyber key is not a nonce.
pub fn registration_challenge_message(nonce: &[u8]) -> Result<Vec<u8>, CryptoError> {
    if nonce.len() != REGISTRATION_CHALLENGE_NONCE_LEN {
        return Err(CryptoError::Protocol(format!(
            "registration challenge nonce must be {} bytes, got {}",
            REGISTRATION_CHALLENGE_NONCE_LEN,
            nonce.len()
        )));
    }
    let mut message = Vec::with_capacity(REGISTRATION_CHALLENGE_DOMAIN.len() + nonce.len());
    message.extend_from_slice(REGISTRATION_CHALLENGE_DOMAIN);
    message.extend_from_slice(nonce);
    Ok(message)
}

/// Domain prefix of a channel membership change signature
/// (`channel.proto`, "Signatures"; server `membership.ChangeDomain`).
pub const CHANNEL_CHANGE_DOMAIN: &[u8] = b"chatter-channel-change-v1\0";

/// Serialise a channel change's `Actions` and sign
/// `"chatter-channel-change-v1\0" || actions` with the identity key. Returns
/// `(actions_bytes, signature)`: the bytes go into `ChannelChange.actions`
/// exactly as signed.
///
/// Like the registration challenge, this is not a general `sign(data)`: the
/// caller hands over a typed `Actions`, never bytes, and the signed message
/// always starts with the domain prefix, so it can never be a prekey
/// (`0x05 || pub`, `0x08 || kyber`) or another protocol's message.
pub fn sign_channel_change(
    identity: &libsignal_protocol::IdentityKeyPair,
    actions: &crate::proto::chat_v1::channel_change::Actions,
) -> Result<(Vec<u8>, Vec<u8>), CryptoError> {
    let bytes = prost::Message::encode_to_vec(actions);
    let mut message = Vec::with_capacity(CHANNEL_CHANGE_DOMAIN.len() + bytes.len());
    message.extend_from_slice(CHANNEL_CHANGE_DOMAIN);
    message.extend_from_slice(&bytes);
    // OS randomness for the XEdDSA nonce, as for the registration challenge.
    use rand::TryRngCore;
    let mut rng = rand::rngs::OsRng.unwrap_err();
    let sig = identity
        .private_key()
        .calculate_signature(&message, &mut rng)
        .map_err(|e| CryptoError::Protocol(e.to_string()))?;
    Ok((bytes, sig.into_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;
    use tempfile::NamedTempFile;

    /// The change signature covers prefix || actions bytes and returns those
    /// exact bytes.
    #[test]
    fn channel_change_signature_is_domain_separated() {
        let identity = IdentityKeyPair::generate(&mut rand::rng());
        let actions = crate::proto::chat_v1::channel_change::Actions {
            channel_id: "c".into(),
            revision: 1,
            ..Default::default()
        };
        let (bytes, sig) = sign_channel_change(&identity, &actions).unwrap();
        assert_eq!(bytes, prost::Message::encode_to_vec(&actions));
        let mut message = CHANNEL_CHANGE_DOMAIN.to_vec();
        message.extend_from_slice(&bytes);
        assert!(identity.public_key().verify_signature(&message, &sig));
        assert!(!identity.public_key().verify_signature(&bytes, &sig));
    }

    fn manager_with_identity() -> (IdentityKeyManager, NamedTempFile) {
        let db_file = NamedTempFile::new().unwrap();
        let db = Arc::new(Database::open(db_file.path(), "test_passphrase").unwrap());
        db.run_migrations().unwrap();
        let mgr = IdentityKeyManager::new(Arc::new(KeyStore::new(db)));
        mgr.generate_identity(&mut rand::rng()).unwrap();
        (mgr, db_file)
    }

    /// A prekey-shaped "nonce" (0x05 || 32-byte public key) is refused before
    /// the identity key is touched — the server cannot use the challenge as a
    /// signed-prekey signing oracle. Same for a Kyber-sized blob and an empty one.
    #[test]
    fn refuses_non_nonce_lengths() {
        let (mgr, _db) = manager_with_identity();
        let mut prekey_shaped = vec![0x05u8];
        prekey_shaped.extend_from_slice(&[0x42; 32]);
        for bad in [prekey_shaped, vec![0x08; 1569], vec![0; 31], Vec::new()] {
            assert!(
                mgr.sign_registration_challenge(&bad).is_err(),
                "signed a {}-byte challenge",
                bad.len()
            );
        }
    }

    /// The signature is over prefix || nonce, never the raw nonce.
    #[test]
    fn signs_domain_separated_message() {
        let (mgr, _db) = manager_with_identity();
        let nonce = [7u8; REGISTRATION_CHALLENGE_NONCE_LEN];
        let sig = mgr.sign_registration_challenge(&nonce).unwrap();
        let public = mgr.get_public_key().unwrap();
        let message = registration_challenge_message(&nonce).unwrap();
        assert!(message.starts_with(b"chatter-reg-challenge-v1\0"));
        assert!(public.public_key().verify_signature(&message, &sig));
        assert!(!public.public_key().verify_signature(&nonce, &sig));
    }
}
