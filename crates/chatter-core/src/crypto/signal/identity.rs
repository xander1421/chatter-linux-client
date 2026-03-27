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

    /// Sign data using the local identity private key.
    ///
    /// Returns the 64-byte Ed25519 signature.
    pub fn sign(&self, data: &[u8]) -> Result<Vec<u8>, CryptoError> {
        let (_, key_pair) = self
            .get_identity()?
            .ok_or(CryptoError::NoIdentity)?;

        key_pair.private_key().calculate_signature(data, &mut rand::rng())
            .map(|sig| sig.into_vec())
            .map_err(|e| CryptoError::Protocol(e.to_string()))
    }
}
